mod receive;
mod send;

use std::{
    collections::{btree_map::Entry, BTreeMap},
    io,
    mem::size_of,
    num::{NonZeroU8, NonZeroUsize},
    time::{Duration, Instant},
};

use num_enum::{IntoPrimitive, TryFromPrimitive};
use thiserror::Error;
use uuid::Uuid;

use self::{receive::ReassembledPacket, send::PacketOut};

/// The maximum size of a packet.
pub const PACKET_BUFFER_SIZE: usize = 512;
/// The max size for the data section of a packet, i.e. without the single kind byte.
pub const PACKET_DATA_SIZE: usize = PACKET_BUFFER_SIZE - size_of::<PacketKind>();

pub type NonZeroBatchSize = NonZeroU8;
pub type BatchSize = u8;

// TODO: Use this instead of raw uuids
pub struct PacketId(Uuid);

/// Sent packets that are not yet fully acked.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SendPacketBuffer {
    packets: BTreeMap<Uuid, PacketOut>,
}

impl SendPacketBuffer {
    /// Creates a new [`SentPacketBuffer`] without any sent packets.
    pub fn new() -> Self {
        Self::default()
    }

    /// Handles incoming packets that are relevant for the sent packet buffer.
    ///
    /// Which is just [`PacketKind::Ack`], other packets are ignored.
    pub fn handle(
        &mut self,
        buffer: &[u8],
        send: impl SendPacket,
    ) -> Result<SentPacketHandling, PacketSendError> {
        let id;
        let seq_index;
        Ok(SentPacketHandling::Ack(
            self.ack(id, seq_index, send)
                .map_err(|kind| PacketSendError { id, kind })?,
        ))
    }

    /// Sends a new packet and returns its id for tracking purposes.
    pub fn send(
        &mut self,
        payload: Vec<u8>,
        send: impl SendPacket,
        initial_batch_size: NonZeroBatchSize,
    ) -> io::Result<Uuid> {
        let id = Uuid::new_v4();
        let packet = PacketOut::send(id, payload, send, initial_batch_size)?;
        self.packets.insert(id, packet);
        Ok(id)
    }

    /// Sends out still pending packet parts.
    pub fn send_pending(
        &mut self,
        send: impl SendPacket,
        resend_delay: Duration,
    ) -> io::Result<()> {
        for (id, packet) in &self.packets {
            packet.send_pending(*id, send, resend_delay)?;
        }
        Ok(())
    }

    /// Returns statistics about the packet with the given id.
    pub fn stats(
        &self,
        id: Uuid,
        resend_delay: Duration,
    ) -> Result<PacketOutStats, InvalidPacketId> {
        let packet = self.packets.get(&id).ok_or(InvalidPacketId { id })?;
        Ok(PacketOutStats {
            total_bytes: packet.total_bytes(),
            acked_bytes: packet.acked_bytes(),
            bytes_per_sec: packet.bytes_per_sec(resend_delay),
            batch_size: packet.batch_size(),
        })
    }

    /// Marks a packet part as acked and returns the overall state.
    ///
    /// Sends out [`PacketKind::Done`] and removes the packet if all parts of the packet were acked.
    fn ack(
        &mut self,
        id: Uuid,
        seq_index: SeqIndex,
        send: impl SendPacket,
    ) -> Result<SentPacketAck, PacketSendErrorKind> {
        let packet = self
            .packets
            .get_mut(&id)
            .ok_or(PacketSendErrorKind::InvalidPacketId)?;
        let ack = packet.ack(seq_index)?;
        if let SentPacketAck::Done = ack {
            let mut done = [0; 17];
            done[0] = PacketKind::Done as PacketKindInt;
            done[1..17].copy_from_slice(id.as_bytes());
            send(&done)?;
            self.packets.remove(&id);
        }
        Ok(ack)
    }

    /// Forget packets that did not receive acks in a long time.
    pub fn timeout_packets(&mut self, timeout: Duration) {
        self.packets
            .retain(|_, packet| packet.awaiting_ack_since().elapsed() < timeout);
    }
}

pub enum SentPacketHandling {
    Unhandled,
    Ack(SentPacketAck),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SentPacketAck {
    Pending { duplicate: bool },
    Done,
}

#[derive(Debug, Error)]
#[error("{kind} (id: {id})")]
pub struct PacketSendError {
    pub id: Uuid,
    pub kind: PacketSendErrorKind,
}

#[derive(Debug, Error)]
pub enum PacketSendErrorKind {
    #[error("invalid packet id")]
    InvalidPacketId,
    #[error(transparent)]
    SeqIndexOutOfRange(#[from] SeqIndexOutOfRange),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Statistics about progress and throughput of a packet.
pub struct PacketOutStats {
    /// The total size of the packet in bytes.
    pub total_bytes: NonZeroUsize,
    /// How many bytes of the packet were sent successfully (i.e. acked).
    pub acked_bytes: usize,
    /// Throughput of this packet measured in bytes per second.
    pub bytes_per_sec: f64,
    /// Up to how many packets are being sent at once.
    pub batch_size: NonZeroBatchSize,
}

/// Received packets to be reassembled.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReceivePacketBuffer {
    packets: BTreeMap<Uuid, PacketIn>,
}

impl ReceivePacketBuffer {
    /// Creates a new [`ReceivedPacketBuffer`] without any received packets.
    pub fn new() -> Self {
        Self::default()
    }

    /// Handles incoming packets that are relevant for the received packet buffer.
    ///
    /// Which is one of:
    ///
    /// - [`PacketKind::SelfContained`]
    /// - [`PacketKind::PartU4`]
    /// - [`PacketKind::PartU8`]
    /// - [`PacketKind::PartU16`]
    /// - [`PacketKind::PartU32`]
    /// - [`PacketKind::Done`]
    ///
    /// Other packets are ignored.
    pub fn handle(
        &mut self,
        buffer: &[u8],
        send: impl SendPacket,
    ) -> Result<ReceivedPacketHandling, HandleReceivedPacketError> {
        let kind = buffer.get(0).expect("payload must not be empty");
        if let Ok(kind) = PacketKind::try_from(*kind) {
            if let PacketKind::Ack | PacketKind::FirstUnreliable = kind {
                return Ok(ReceivedPacketHandling::Unhandled);
            }

            let id = Uuid::from_slice(
                buffer
                    .get(1..17)
                    .ok_or(HandleReceivedPacketError::Malformed)?,
            )
            .unwrap();

            if kind == PacketKind::Done {
                self.done(id)?;
                return Ok(ReceivedPacketHandling::Done);
            }
            let seq_kind = match kind {
                PacketKind::SelfContained => SeqKind::None,
                PacketKind::PartU4 => SeqKind::U4,
                PacketKind::PartU8 => SeqKind::U8,
                PacketKind::PartU16 => SeqKind::U16,
                PacketKind::PartU32 => SeqKind::U32,
                PacketKind::Ack | PacketKind::Done | PacketKind::FirstUnreliable => unreachable!(),
            };

            let seq_index;
            let seq_max;
            let payload = &buffer[todo!()..];

            Ok(ReceivedPacketHandling::Received(self.receive(
                id, seq_kind, seq_index, seq_max, payload, send,
            )?))
        } else {
            Ok(ReceivedPacketHandling::Unhandled)
        }
    }

    /// Reassembles the given packet and returns its state.
    pub fn receive(
        &mut self,
        id: Uuid,
        seq_kind: SeqKind,
        seq_index: SeqIndex,
        seq_max: SeqIndex,
        payload: &[u8],
        send: impl SendPacket,
    ) -> Result<ReceivedPacketAck, PacketReceiveError> {
        if seq_max == 0 {
            match self.packets.entry(id) {
                Entry::Vacant(entry) => {
                    entry.insert(PacketIn {
                        last_receive: Instant::now(),
                        packet: None,
                    });
                    ack(id, seq_index, send)?;
                    Ok(ReceivedPacketAck::Done(payload))
                }
                Entry::Occupied(entry) => {
                    if entry.get().packet.is_none() {
                        ack(id, seq_index, send)?;
                        Ok(ReceivedPacketAck::Pending { duplicate: true })
                    } else {
                        Err(PacketReceiveError {
                            id,
                            kind: PacketReceiveErrorKind::Mismatch,
                        })
                    }
                }
            }
        } else {
            let packet = self.packets.entry(id).or_insert_with(|| PacketIn {
                last_receive: Instant::now(),
                packet: Some(ReassembledPacket::new(seq_kind, seq_max)),
            });
            if let Some(packet) = &mut packet.packet {
                let packet_ack = packet
                    .receive(id, seq_kind, seq_index, payload)
                    .map_err(|kind| PacketReceiveError { id, kind })?;
                ack(id, seq_index, send)?;
                Ok(packet_ack)
            } else {
                Err(PacketReceiveError {
                    id,
                    kind: PacketReceiveErrorKind::Mismatch,
                })
            }
        }
    }

    /// The sender is aware that all packets have been received, so the id can be removed.
    pub fn done(&mut self, id: Uuid) -> Result<(), InvalidPacketId> {
        if self.packets.remove(&id).is_some() {
            Ok(())
        } else {
            Err(InvalidPacketId { id })
        }
    }

    pub fn stats(&self, id: Uuid) -> PacketInStats {
        todo!()
    }

    /// Forget packets that did not receive anything in a long time.
    pub fn timeout_packets(&mut self, timeout: Duration) {
        self.packets
            .retain(|_, packet| packet.last_receive.elapsed() < timeout);
    }
}

pub struct PacketInStats {
    pub total_bytes: NonZeroUsize,
    pub received_bytes: NonZeroUsize,
}

pub enum ReceivedPacketHandling<'a> {
    /// This packet should be handled by someone else.
    Unhandled,
    /// This packet was received and acked.
    Received(ReceivedPacketAck<'a>),
    /// The sender is aware that all packets have been received, so the receiver can forget it.
    Done,
}

pub enum ReceivedPacketAck<'a> {
    /// A part of the packet was received and acked.
    Pending {
        /// Whether this packet was already received, which can happen when an ack gets dropped.
        duplicate: bool,
    },
    /// The packet is fully assembled.
    Done(&'a [u8]),
}

#[derive(Debug, Error)]
#[error("invalid packet id: {id}")]
pub struct InvalidPacketId {
    pub id: Uuid,
}

#[derive(Debug, Error)]
pub enum HandleReceivedPacketError {
    #[error("malformed packet")]
    Malformed,
    #[error(transparent)]
    Receive(#[from] PacketReceiveError),
    #[error(transparent)]
    InvalidPacketId(#[from] InvalidPacketId),
    #[error(transparent)]
    Io(#[from] io::Error),
}

#[derive(Debug, Error)]
#[error("{kind} (id: {id})")]
pub struct PacketReceiveError {
    pub id: Uuid,
    pub kind: PacketReceiveErrorKind,
}

#[derive(Debug, Error)]
#[error("the packet sequence index is out of range ({seq_index} > {seq_max})")]
pub struct SeqIndexOutOfRange {
    pub seq_index: SeqIndex,
    pub seq_max: SeqIndex,
}

#[derive(Debug, Error)]
pub enum PacketReceiveErrorKind {
    #[error("a packet with this id already exists with a different type")]
    Mismatch,
    #[error(transparent)]
    SeqIndexOutOfRange(#[from] SeqIndexOutOfRange),
    #[error(transparent)]
    Io(#[from] io::Error),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PacketIn {
    last_receive: Instant,
    packet: Option<ReassembledPacket>,
}

trait SendPacket: Fn(&[u8]) -> io::Result<usize> {}
impl<T: Fn(&[u8]) -> io::Result<usize>> SendPacket for T {}

type PacketKindInt = u8;

#[derive(Clone, Copy, Debug, PartialEq, Eq, IntoPrimitive, TryFromPrimitive)]
#[repr(u8)]
enum PacketKind {
    /// A self-contained (up to ~500 bytes) reliable packet.
    SelfContained,
    /// A single part of an up to ~8KiB reliable packet.
    PartU4,
    /// A single part of an up to ~128KiB reliable packet.
    PartU8,
    /// A single part of an up to ~32MiB reliable packet.
    PartU16,
    /// A single part of an up to ~2TiB reliable packet.
    PartU32,
    /// Confirms that a reliable packet (or part) has been received.
    Ack,
    /// Confirms that all acks of a reliable packet were received by the sender.
    ///
    /// This allows the receiver to forget this uuid, since the sender guarantees to never send a
    /// packet with this uuid again.
    ///
    /// If this packet gets dropped, the receiver would keep the uuid around indefinitely. For this
    /// reason, the receiver also forgets uuids after a sufficiently long timeout.
    Done,
    /// Not a packet kind, but instead marks the start of the first user packet.
    ///
    /// These are unreliable and self-contained.
    FirstUnreliable,
}

/// Can store a single sequence index.
type SeqIndex = u32;

/// Can store both sequence index and max for any size.
type SeqIndices = u64;

/// What size to use for the sequence index and max.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SeqKind {
    None = 0,
    U4 = 4,
    U8 = 8,
    U16 = 16,
    U32 = 32,
}

impl SeqKind {
    /// All sequence index kinds in order of increasing potential packet size.
    const SORTED_BY_MAX_BATCH_SIZE: [Self; 5] =
        [Self::None, Self::U4, Self::U8, Self::U16, Self::U32];

    /// The number of bits used to store a single sequence index.
    ///
    /// The total count must be stored as a max index to fit!
    fn bits(self) -> usize {
        self as usize
    }

    /// The maximum number of chunks that can be sent in a batch using this kind of sequence index.
    fn max_batch_size(self) -> usize {
        1 << self.bits()
    }

    /// The packet kind identifying this kind of sequence index.
    fn packet_kind(self) -> PacketKind {
        match self {
            Self::None => PacketKind::SelfContained,
            Self::U4 => PacketKind::PartU4,
            Self::U8 => PacketKind::PartU8,
            Self::U16 => PacketKind::PartU16,
            Self::U32 => PacketKind::PartU32,
        }
    }

    /// How big a the data section of a chunk with this kind of sequence index is.
    const fn chunk_size(self) -> usize {
        PACKET_DATA_SIZE - size_of::<Uuid>() - self.bits() / 4
    }
}

fn ack(id: Uuid, seq_index: u32, send: impl SendPacket) -> Result<(), PacketReceiveError> {
    let mut ack = [0; 17 + size_of::<SeqIndex>()];
    ack[0] = PacketKind::Ack as u8;
    ack[1..17].copy_from_slice(id.as_bytes());
    ack[17..17 + size_of::<SeqIndex>()].copy_from_slice(&seq_index.to_le_bytes());
    send(&ack).map_err(|error| PacketReceiveError {
        id,
        kind: error.into(),
    })?;
    Ok(())
}
