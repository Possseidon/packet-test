mod receive;
mod send;
mod serialize;

use std::{
    collections::{btree_map::Entry, BTreeMap},
    io,
    marker::PhantomData,
    mem::size_of,
    num::{NonZeroU16, NonZeroUsize},
    time::{Duration, Instant},
};

use num_enum::{IntoPrimitive, TryFromPrimitive};
use rkyv::{ser::serializers::BufferSerializer, AlignedBytes, Archive, Serialize};
use thiserror::Error;
use uuid::Uuid;

use self::{
    receive::{ReassembledPacket, ReceivedPacketAck},
    send::{PacketOut, PacketQueue},
    serialize::ChannelSerializer,
};

/// The maximum size of a packet.
pub const PACKET_BUFFER_SIZE: usize = 512;
/// The max size for the data section of a packet, i.e. without the single kind byte.
pub const PACKET_DATA_SIZE: usize = PACKET_BUFFER_SIZE - size_of::<PacketKind>();

pub type NonZeroBufferIndex = NonZeroU16;
pub type BufferIndex = u16;

/// Various packet types that are used by server and client to communicate with each other.
pub trait Packets {
    /// Packets sent by the server.
    type Server: Packet;
    /// Packets sent by the client.
    type Client: Packet;

    /// Update packets sent by the server.
    type ServerUpdate: UpdatePacket;
    /// Update packets sent by the client.
    type ClientUpdate: UpdatePacket;
}

/// Contains only the necessary packets to establish a connection between server and client.
struct DefaultPackets;

impl Packets for DefaultPackets {
    type Server = ();
    type Client = ();

    type ServerUpdate = NoUpdate;
    type ClientUpdate = NoUpdate;
}

pub trait Packet:
    Serialize<BufferSerializer<[AlignedBytes<PACKET_BUFFER_SIZE>; 15]>>
    + Serialize<ChannelSerializer>
    + Send
    + 'static
{
}

impl<
        T: Serialize<BufferSerializer<[AlignedBytes<PACKET_BUFFER_SIZE>; 15]>>
            + Serialize<ChannelSerializer>
            + Send
            + 'static,
    > Packet for T
{
}

/// Server packets for dealing with client connections.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Archive, Serialize)]
pub enum ServerConnectionPacket<T> {
    Accept,
    Reject,
    Kick,
    User(T),
}

/// Client packets for establishing a server connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Archive, Serialize)]
pub enum ClientConnectionPacket<T> {
    Connect,
    Disconnect,
    User(T),
}

/// Packets for things that constantly change, which means it's okay if some are dropped.
///
/// These packets must fit into a single datagram.
#[allow(clippy::len_without_is_empty)] // len is always > 0
pub trait UpdatePacket: Sized {
    type ReadError: std::error::Error;

    /// Lower bound for the size of the packet.
    ///
    /// Used as an upfront check to see if the packet might fit in the remaining buffer.
    ///
    /// Must be at most [`PACKET_BUFFER_SIZE`].
    fn len(&self) -> NonZeroBufferIndex;

    /// Writes [`UpdatePacket::len()`] bytes to the given buffer.
    ///
    /// The first byte must be at least [`PacketKind::FirstUpdate`].
    fn write(&self, buf: &mut [u8]);

    /// Tries to read a packet from the given buffer.
    ///
    /// The returned value can be used to get the number of read bytes via [`UpdatePacket::len()`].
    fn read(buf: &[u8]) -> Result<Self, Self::ReadError>;
}

pub enum NoUpdate {}

#[derive(Debug, Error)]
#[error("there are no update packets")]
pub struct NoUpdateError;

impl UpdatePacket for NoUpdate {
    type ReadError = NoUpdateError;

    fn len(&self) -> NonZeroBufferIndex {
        unreachable!()
    }

    fn write(&self, _buf: &mut [u8]) {
        unreachable!()
    }

    fn read(_buf: &[u8]) -> Result<Self, Self::ReadError> {
        Err(NoUpdateError)
    }
}

pub type NonZeroBatchSize = NonZeroU16;
pub type BatchSize = u16;

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct PacketId(Uuid);

impl PacketId {
    fn new() -> Self {
        Self(Uuid::new_v4())
    }

    fn as_bytes(&self) -> &[u8; 16] {
        self.0.as_bytes()
    }

    fn from_slice(bytes: &[u8]) -> Option<Self> {
        Uuid::from_slice(bytes).ok().map(Self)
    }
}

pub struct PacketBuffers<S: Packet, R: Packet> {
    send_buffer: SendPacketBuffer,
    receive_buffer: ReceivePacketBuffer,
    _phantom: PhantomData<fn() -> (S, R)>,
}

impl<S: Packet, R: Packet> PacketBuffers<S, R> {
    pub fn new(initial_send_batch_size: NonZeroBatchSize) -> Self {
        Self {
            send_buffer: SendPacketBuffer::new(initial_send_batch_size),
            receive_buffer: Default::default(),
            _phantom: PhantomData,
        }
    }

    pub fn send(&mut self, packet: &S, send: impl SendPacket) -> io::Result<PacketId> {
        todo!()
    }
}

impl<S: Packet, R: Packet> std::fmt::Debug for PacketBuffers<S, R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PacketBuffers")
            .field("send_buffer", &self.send_buffer)
            .field("receive_buffer", &self.receive_buffer)
            .finish()
    }
}

/// Sent packets that are not yet fully acked.
#[derive(Debug)]
struct SendPacketBuffer {
    // TODO: Update batch_size somehow
    batch_size: NonZeroBatchSize,
    packets: BTreeMap<PacketId, PacketOut>,
}

impl SendPacketBuffer {
    /// Creates a new [`SentPacketBuffer`] without any sent packets.
    fn new(initial_batch_size: NonZeroBatchSize) -> Self {
        Self {
            batch_size: initial_batch_size,
            packets: Default::default(),
        }
    }

    /// Handles incoming packets that are relevant for the sent packet buffer.
    ///
    /// Which is just [`PacketKind::Ack`], other packets are ignored.
    fn handle(
        &mut self,
        buffer: &[u8],
        send: impl SendPacket,
    ) -> Result<SentPacketHandling, PacketSendError> {
        todo!();

        // let id;
        // let seq_index;
        // Ok(SentPacketHandling::Ack(
        //     self.ack(id, seq_index, send)
        //         .map_err(|kind| PacketSendError { id, kind })?,
        // ))
    }

    /// Sends a new packet and returns its id for tracking purposes.
    fn send(
        &mut self,
        packet_queue: impl Into<PacketQueue>,
        send: impl SendPacket,
    ) -> io::Result<PacketId> {
        let id = PacketId::new();
        self.packets
            .insert(id, PacketOut::send(packet_queue, send, self.batch_size)?);
        Ok(id)
    }

    /// Sends out still pending packet parts.
    fn send_pending(&mut self, send: impl SendPacket, resend_delay: Duration) -> io::Result<()> {
        for (id, packet) in &mut self.packets {
            packet.send_pending(*id, &send, resend_delay)?;
        }
        Ok(())
    }

    /// Returns statistics about the packet with the given id.
    fn stats(
        &self,
        id: PacketId,
        resend_delay: Duration,
    ) -> Result<PacketOutStats, InvalidPacketId> {
        let packet = self.packets.get(&id).ok_or(InvalidPacketId { id })?;
        Ok(PacketOutStats {
            // total_bytes: packet.total_bytes(),
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
        id: PacketId,
        seq_index: SeqIndex,
        send: impl SendPacket,
    ) -> Result<bool, PacketSendErrorKind> {
        Ok(self
            .packets
            .get_mut(&id)
            .ok_or(PacketSendErrorKind::InvalidPacketId)?
            .ack(seq_index)?)
    }

    /// Forget packets that did not receive acks in a long time.
    fn timeout_packets(&mut self, timeout: Duration) {
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
#[error("{id:?} {kind}")]
pub struct PacketSendError {
    pub id: PacketId,
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
    #[error(transparent)]
    SeqIndexAckedBeforeSent(#[from] SeqIndexAckedBeforeSent),
}

/// Statistics about progress and throughput of a packet.
pub struct PacketOutStats {
    /// The total size of the packet in bytes.
    // pub total_bytes: NonZeroUsize,
    /// How many bytes of the packet were sent successfully (i.e. acked).
    pub acked_bytes: usize,
    /// Throughput of this packet measured in bytes per second.
    pub bytes_per_sec: f64,
    /// Up to how many packets are being sent at once.
    pub batch_size: NonZeroBatchSize,
}

/// Received packets to be reassembled.
#[derive(Clone, Debug, Default)]
struct ReceivePacketBuffer {
    packets: BTreeMap<PacketId, PacketIn>,
}

impl ReceivePacketBuffer {
    /// Creates a new [`ReceivedPacketBuffer`] without any received packets.
    fn new() -> Self {
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
    fn handle(
        &mut self,
        buffer: &[u8],
        send: impl SendPacket,
    ) -> Result<ReceivedPacketHandling, HandleReceivedPacketError> {
        todo!();

        // let kind = buffer.get(0).expect("payload must not be empty");
        // if let Ok(kind) = PacketKind::try_from(*kind) {
        //     if let PacketKind::Ack | PacketKind::FirstUpdate = kind {
        //         return Ok(ReceivedPacketHandling::Unhandled);
        //     }

        //     let id = PacketId::from_slice(
        //         buffer
        //             .get(1..17)
        //             .ok_or(HandleReceivedPacketError::Malformed)?,
        //     )
        //     .unwrap();

        //     if kind == PacketKind::Done {
        //         self.done(id)?;
        //         return Ok(ReceivedPacketHandling::Done);
        //     }
        //     let seq_kind = match kind {
        //         PacketKind::SelfContained => SeqKind::None,
        //         PacketKind::PartU4 => SeqKind::U4,
        //         PacketKind::PartU8 => SeqKind::U8,
        //         PacketKind::PartU16 => SeqKind::U16,
        //         PacketKind::PartU32 => SeqKind::U32,
        //         PacketKind::Ack | PacketKind::Done | PacketKind::FirstUpdate => unreachable!(),
        //     };

        //     let seq_index;
        //     let seq_max;
        //     let payload = &buffer[todo!()..];

        //     Ok(ReceivedPacketHandling::Received(self.receive(
        //         id, seq_kind, seq_index, seq_max, payload, send,
        //     )?))
        // } else {
        //     Ok(ReceivedPacketHandling::Unhandled)
        // }
    }

    /// Reassembles the given packet and returns its state.
    fn receive<'a>(
        &'a mut self,
        last: bool,
        id: PacketId,
        seq_index: SeqIndex,
        payload: &'a [u8],
        send: impl SendPacket,
    ) -> Result<ReceivedPacketAck<'a>, PacketReceiveError> {
        if last && seq_index == 0 {
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
                packet: Some(ReassembledPacket::new()),
            });
            if let Some(packet) = &mut packet.packet {
                let packet_ack = packet.receive(last, seq_index, payload);
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
    fn done(&mut self, id: PacketId) -> Result<(), InvalidPacketId> {
        if self.packets.remove(&id).is_some() {
            Ok(())
        } else {
            Err(InvalidPacketId { id })
        }
    }

    fn stats(&self, id: PacketId) -> PacketInStats {
        todo!()
    }

    /// Forget packets that did not receive anything in a long time.
    fn timeout_packets(&mut self, timeout: Duration) {
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

#[derive(Debug, Error)]
#[error("invalid {id:?}")]
pub struct InvalidPacketId {
    pub id: PacketId,
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
#[error("{id:?} {kind}")]
pub struct PacketReceiveError {
    pub id: PacketId,
    pub kind: PacketReceiveErrorKind,
}

#[derive(Debug, Error)]
#[error("the packet sequence index is out of range ({seq_index} > {seq_max})")]
pub struct SeqIndexOutOfRange {
    pub seq_index: SeqIndex,
    pub seq_max: SeqIndex,
}

#[derive(Debug, Error)]
#[error("the packet sequence index {seq_index} got acked before it sent")]
pub struct SeqIndexAckedBeforeSent {
    pub seq_index: SeqIndex,
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

#[derive(Clone, Debug)]
struct PacketIn {
    last_receive: Instant,
    packet: Option<ReassembledPacket>,
}

trait SendPacket: Fn(&[u8]) -> io::Result<usize> {}
impl<T: Fn(&[u8]) -> io::Result<usize>> SendPacket for T {}

#[derive(Clone, Copy, Debug, PartialEq, Eq, IntoPrimitive, TryFromPrimitive)]
#[repr(u8)]
enum PacketKind {
    /// A part of a reliable packet.
    Part,
    /// The last part of a reliable packet.
    LastPart,
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
    /// Not a packet kind, but instead marks the start of the first [`UpdatePacket`].
    FirstUpdate,
}

/// How much payload a part packet can hold at most.
///
/// Everything except kind, id and seq_index.
const PART_PACKET_PAYLOAD_SIZE: usize =
    PACKET_DATA_SIZE - size_of::<PacketId>() - size_of::<SeqIndex>();

/// Can store a single sequence index.
type SeqIndex = u32;

fn ack(id: PacketId, seq_index: SeqIndex, send: impl SendPacket) -> Result<(), PacketReceiveError> {
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
