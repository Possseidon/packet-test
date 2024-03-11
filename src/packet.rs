pub(crate) mod receive;
pub(crate) mod send;
pub(crate) mod serialize;

use std::{
    collections::{btree_map::Entry, BTreeMap},
    convert::Infallible,
    io,
    marker::PhantomData,
    mem::size_of,
    num::{NonZeroU16, NonZeroUsize},
    time::{Duration, Instant},
};

use num_enum::{IntoPrimitive, TryFromPrimitive};
use rkyv::{Archive, CheckBytes, Fallible, Serialize};
use thiserror::Error;
use uuid::Uuid;

use self::{
    receive::{ReassembledPacket, ReceivedPacket},
    send::PacketOut,
    serialize::{serialize_packet, ChannelPacketSerializer, VecPacketSerializer},
};

// TODO: Consider making PACKET_BUFFER_SIZE dynamic
//       That way it can be adjusted per client if e.g. some clients support really big packets
//       At the same time, while 512 is a pretty good value, it might be too big in some rare cases
//       Also, use u16, since that's roughly the maximum theoretical possible size of a packet

/// The maximum size of a packet.
pub const PACKET_BUFFER_SIZE: usize = 512;
/// The max size for the data section of a packet, i.e. without the single kind byte.
pub const PACKET_DATA_SIZE: usize = PACKET_BUFFER_SIZE - size_of::<PacketKind>();

pub type NonZeroBufferIndex = NonZeroU16;
pub type BufferIndex = u16;

/// Various packet types that are used by server and client to communicate with each other.
pub trait Packets: 'static {
    /// Sent out by the client to query status information from a server.
    ///
    /// Can also be used to discover servers by sending a broadcast.
    type Query: Packet;
    /// Sent out by the server in response to a status query request.
    type Status: Packet;

    /// Sent out by the client in an attempt to connect to a server, followed up by accept/reject.
    type Connect: Packet;
    /// Sent out by the client to notify the server that it has disconnected.
    type Disconnect: Packet;

    /// Sent out by the server to notify a client that it has been accepted and is now connected.
    type Accept: Packet;
    /// Sent out by the server to notify a client that it has been rejected.
    type Reject: Packet;
    /// Sent out by the server to notify a client that it has been kicked and is now disconnected.
    type Kick: Packet;

    /// User-defined packet sent by the server.
    type Server: Packet;
    /// User-defined packet sent by the client.
    type Client: Packet;

    /// User-defined update packet sent by the server.
    type ServerUpdate: UpdatePacket;
    /// User-defined update packet sent by the client.
    type ClientUpdate: UpdatePacket;
}

/// Contains only the necessary packets to establish a connection between server and client.
pub struct DefaultPackets;

impl Packets for DefaultPackets {
    type Query = ();
    type Status = ();

    type Connect = ();
    type Disconnect = ();

    type Accept = ();
    type Reject = ();
    type Kick = ();

    type Server = NoPacket;
    type Client = NoPacket;

    type ServerUpdate = NoPacket;
    type ClientUpdate = NoPacket;
}

pub trait Packet:
    Serialize<VecPacketSerializer> + Serialize<ChannelPacketSerializer> + Send + 'static
{
}

impl<T: Serialize<VecPacketSerializer> + Serialize<ChannelPacketSerializer> + Send + 'static> Packet
    for T
{
}

/// Server packets for dealing with client connections.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Archive, Serialize)]
#[archive(check_bytes)]
pub(crate) enum ServerConnectionPacket<P: Packets> {
    Status(P::Status),
    Accept(P::Accept),
    Reject(P::Reject),
    Kick(P::Kick),
    User(P::Server),
}

/// Client packets for establishing a server connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Archive, Serialize)]
#[archive(check_bytes)]
pub(crate) enum ClientConnectionPacket<P: Packets> {
    Query(P::Query),
    Connect(P::Connect),
    Disconnect(P::Disconnect),
    User(P::Client),
}

pub(crate) type ServerPacketBuffers<P> =
    PacketBuffers<ServerConnectionPacket<P>, ClientConnectionPacket<P>>;
pub(crate) type ClientPacketBuffers<P> =
    PacketBuffers<ClientConnectionPacket<P>, ServerConnectionPacket<P>>;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoPacket {}

impl Archive for NoPacket {
    type Archived = ArchivedNoPacket;
    type Resolver = ();

    unsafe fn resolve(&self, pos: usize, resolver: Self::Resolver, out: *mut Self::Archived) {
        unreachable!();
    }
}

impl<S: Fallible + ?Sized> Serialize<S> for NoPacket {
    fn serialize(&self, serializer: &mut S) -> Result<Self::Resolver, S::Error> {
        unreachable!()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArchivedNoPacket {}

impl<C: ?Sized> CheckBytes<C> for ArchivedNoPacket {
    type Error = Infallible;

    unsafe fn check_bytes<'a>(
        value: *const Self,
        context: &mut C,
    ) -> Result<&'a Self, Self::Error> {
        unreachable!()
    }
}

#[derive(Debug, Error)]
#[error("there are no update packets")]
pub struct NoUpdateError;

impl UpdatePacket for NoPacket {
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

/// Connection related configuration for both server and client.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConnectionConfig {
    /// How long to wait for a packet before disconnecting.
    pub timeout: Duration,
    /// How long to wait for an ack before the packet is resent.
    pub resend_delay: Duration,
    /// How many packets are sent at once initially for new connections.
    ///
    /// This value is adjusted automatically (on a per-connection basis for servers).
    pub initial_send_batch_size: NonZeroBatchSize,
    /// A packet that requires more than this number of chunks, will serialize in the background.
    ///
    /// This spawns a background thread and communicates back via a rendezvous channel.
    pub background_serialization_threshold: usize,
}

impl Default for ConnectionConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(10),
            resend_delay: Duration::from_secs(1),
            initial_send_batch_size: NonZeroBatchSize::new(8).unwrap(),
            background_serialization_threshold: 8,
        }
    }
}

/// Server specific configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ServerConfig {
    /// How long to wait between each ping.
    ///
    /// Should be considerably less than [`ConnectionConfig::timeout`] to avoid timeouts when there
    /// aren't any other packets being sent for a while.
    pub ping_delay: Duration,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            ping_delay: Duration::from_secs(3),
        }
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

pub(crate) struct PacketBuffers<S: Packet, R: Packet> {
    send_buffer: SendPacketBuffer,
    receive_buffer: ReceivePacketBuffer,
    _phantom: PhantomData<fn() -> (S, R)>,
}

impl<S: Packet, R: Packet> PacketBuffers<S, R> {
    pub(crate) fn new(initial_send_batch_size: NonZeroBatchSize) -> Self {
        Self {
            send_buffer: SendPacketBuffer::new(initial_send_batch_size),
            receive_buffer: Default::default(),
            _phantom: PhantomData,
        }
    }

    pub(crate) fn send(
        &mut self,
        packet: S,
        background_serialization_threshold: usize,
        send: impl SendPacket,
    ) -> io::Result<PacketId> {
        self.send_buffer
            .send(packet, background_serialization_threshold, send)
    }

    pub(crate) fn update(
        &mut self,
        timeout: Duration,
        resend_delay: Duration,
        send: impl SendPacket,
    ) -> io::Result<()> {
        self.timeout_packets(timeout);
        self.send_pending(resend_delay, &send)?;
        Ok(())
    }

    pub(crate) fn handle<'a>(
        &'a mut self,
        buf: &'a [u8],
        send: impl SendPacket,
    ) -> Result<PacketHandling<'a>, HandlePacketError> {
        match self.receive_buffer.handle(buf, &send)? {
            ReceivedPacketHandling::Ack(received) => return Ok(PacketHandling::Received(received)),
            ReceivedPacketHandling::Done => return Ok(PacketHandling::Done),
            ReceivedPacketHandling::Unhandled => {}
        }

        match self.send_buffer.handle(buf)? {
            SentPacketHandling::Ack { duplicate } => return Ok(PacketHandling::Ack { duplicate }),
            SentPacketHandling::Unhandled => {}
        }

        // TODO: Handle update packets

        Err(HandlePacketError::UnexpectedKind(42)) // TODO: extract kind and use it
    }

    fn send_pending(&mut self, resend_delay: Duration, send: impl SendPacket) -> io::Result<()> {
        self.send_buffer.send_pending(resend_delay, send)
    }

    fn timeout_packets(&mut self, timeout: Duration) {
        self.send_buffer.timeout_packets(timeout);
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

pub(crate) enum PacketHandling<'a> {
    /// A part of a packet was received.
    Received(ReceivedPacket<'a>),
    /// The sender is aware that all packets have been received, so the receiver can forget it.
    Done,
    /// A part of a sent packet was acked by the receiver.
    Ack { duplicate: bool },
}

/// Sent packets that are not yet fully acked.
#[derive(Debug)]
struct SendPacketBuffer {
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
    fn handle(&mut self, buf: &[u8]) -> Result<SentPacketHandling, HandlePacketError> {
        let kind = buf.first().ok_or(HandlePacketError::Empty)?;
        if let Ok(kind) = PacketKind::try_from(*kind) {
            match kind {
                PacketKind::Ack => {
                    let id_start = size_of::<PacketKind>();
                    let id_end = id_start + size_of::<PacketId>();
                    let seq_index_end = id_end + size_of::<SeqIndex>();

                    let id = PacketId::from_slice(
                        buf.get(id_start..id_end)
                            .ok_or(HandlePacketError::Malformed)?,
                    )
                    .unwrap();
                    let seq_index = SeqIndex::from_le_bytes(
                        buf.get(id_end..seq_index_end)
                            .ok_or(HandlePacketError::Malformed)?
                            .try_into()
                            .unwrap(),
                    );

                    Ok(SentPacketHandling::Ack {
                        duplicate: self
                            .ack(id, seq_index)
                            .map_err(|kind| PacketAckError { id, kind })?,
                    })
                }
                PacketKind::Part
                | PacketKind::LastPart
                | PacketKind::Done
                | PacketKind::FirstUpdate => Ok(SentPacketHandling::Unhandled),
            }
        } else {
            Ok(SentPacketHandling::Unhandled)
        }
    }

    /// Sends a new packet and returns its id for tracking purposes.
    fn send(
        &mut self,
        packet: impl Packet,
        background_serialization_threshold: usize,
        send: impl SendPacket,
    ) -> io::Result<PacketId> {
        let id = PacketId::new();
        println!("Sending {id:?}");
        self.packets.insert(
            id,
            PacketOut::send(
                serialize_packet(id, background_serialization_threshold, packet),
                send,
                self.batch_size,
            )?,
        );
        Ok(id)
    }

    /// Sends out still pending packet parts.
    fn send_pending(&mut self, resend_delay: Duration, send: impl SendPacket) -> io::Result<()> {
        let mut batch_size: usize = 0;
        let mut count: usize = 0;
        for (id, packet) in &mut self.packets {
            packet.send_pending(*id, resend_delay, &send)?;
            batch_size = batch_size.saturating_add(usize::from(packet.batch_size().get()));
            count += 1;
        }
        if count != 0 {
            let batch_size = BatchSize::try_from(batch_size / count).unwrap_or(BatchSize::MAX);
            self.batch_size = NonZeroBatchSize::new(batch_size).unwrap_or(NonZeroBatchSize::MIN);
        }
        Ok(())
    }

    /// Returns statistics about the packet with the given id.
    fn stats(&self, id: PacketId, resend_delay: Duration) -> Option<PacketOutStats> {
        let packet = self.packets.get(&id)?;
        Some(PacketOutStats {
            // total_bytes: packet.total_bytes(),
            acked_bytes: packet.acked_bytes(),
            bytes_per_sec: packet.bytes_per_sec(resend_delay),
            batch_size: packet.batch_size(),
        })
    }

    /// Marks a packet part as acked and returns the overall state.
    fn ack(&mut self, id: PacketId, seq_index: SeqIndex) -> Result<bool, PacketAckErrorKind> {
        Ok(self
            .packets
            .get_mut(&id)
            .ok_or(PacketAckErrorKind::InvalidPacketId)?
            .ack(seq_index)?)
    }

    /// Forget packets that did not receive acks in a long time.
    fn timeout_packets(&mut self, timeout: Duration) {
        self.packets.retain(|id, packet| {
            let within_timeout = packet.awaiting_ack_since().elapsed() < timeout;
            if !within_timeout {
                println!("{id:?} timed out");
            }
            within_timeout
        });
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SentPacketHandling {
    /// A sent out packet has been acked.
    Ack {
        /// Whether the packet was already acked before.
        duplicate: bool,
    },
    /// This packet should be handled by someone else.
    Unhandled,
}

#[derive(Debug, Error)]
#[error("{id:?} {kind}")]
pub struct PacketAckError {
    pub id: PacketId,
    pub kind: PacketAckErrorKind,
}

#[derive(Debug, Error)]
pub enum PacketAckErrorKind {
    #[error("invalid packet id")]
    InvalidPacketId,
    #[error(transparent)]
    SeqIndexOutOfRange(#[from] SeqIndexOutOfRange),
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
    /// Handles incoming packets that are relevant for the received packet buffer.
    ///
    /// Which is one of:
    ///
    /// - [`PacketKind::Part`]
    /// - [`PacketKind::LastPart`]
    /// - [`PacketKind::Done`]
    ///
    /// Other packets are ignored.
    fn handle<'a>(
        &'a mut self,
        buf: &'a [u8],
        send: impl SendPacket,
    ) -> Result<ReceivedPacketHandling<'a>, HandlePacketError> {
        let kind = buf.first().ok_or(HandlePacketError::Empty)?;
        if let Ok(kind) = PacketKind::try_from(*kind) {
            let id_start = size_of::<PacketKind>();
            let id_end = id_start + size_of::<PacketId>();

            let id = || -> Result<_, HandlePacketError> {
                Ok(PacketId::from_slice(
                    buf.get(id_start..id_end)
                        .ok_or(HandlePacketError::Malformed)?,
                )
                .unwrap())
            };

            match kind {
                PacketKind::Part | PacketKind::LastPart => {
                    let seq_index_end = id_end + size_of::<SeqIndex>();
                    let seq_index = SeqIndex::from_le_bytes(
                        buf.get(id_end..seq_index_end)
                            .ok_or(HandlePacketError::Malformed)?
                            .try_into()
                            .unwrap(),
                    );

                    let payload = buf
                        .get(seq_index_end..)
                        .ok_or(HandlePacketError::Malformed)?;

                    Ok(ReceivedPacketHandling::Ack(self.receive(
                        kind == PacketKind::LastPart,
                        id()?,
                        seq_index,
                        payload,
                        send,
                    )?))
                }
                PacketKind::Done => {
                    self.done(id()?)?;
                    Ok(ReceivedPacketHandling::Done)
                }
                PacketKind::Ack | PacketKind::FirstUpdate => Ok(ReceivedPacketHandling::Unhandled),
            }
        } else {
            Ok(ReceivedPacketHandling::Unhandled)
        }
    }

    /// Reassembles the given packet and returns its state.
    fn receive<'a>(
        &'a mut self,
        last: bool,
        id: PacketId,
        seq_index: SeqIndex,
        payload: &'a [u8],
        send: impl SendPacket,
    ) -> Result<ReceivedPacket<'a>, PacketReceiveError> {
        if last && seq_index == 0 {
            match self.packets.entry(id) {
                Entry::Vacant(entry) => {
                    entry.insert(PacketIn {
                        last_receive: Instant::now(),
                        packet: None,
                    });
                    ack(id, seq_index, send)?;
                    Ok(ReceivedPacket::Reassembled(payload))
                }
                Entry::Occupied(entry) => {
                    if entry.get().packet.is_none() {
                        ack(id, seq_index, send)?;
                        Ok(ReceivedPacket::Pending { duplicate: true })
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
                let received_packet = packet.receive(last, seq_index, payload);
                ack(id, seq_index, send)?;
                Ok(received_packet)
            } else {
                Err(PacketReceiveError {
                    id,
                    kind: PacketReceiveErrorKind::Mismatch,
                })
            }
        }
    }

    /// The sender is aware that all packets have been received, so the id can be removed.
    fn done(&mut self, id: PacketId) -> Result<(), InvalidDonePacketId> {
        if self.packets.remove(&id).is_some() {
            Ok(())
        } else {
            Err(InvalidDonePacketId { id })
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

pub(crate) enum ReceivedPacketHandling<'a> {
    /// A part of a packet was acked.
    Ack(ReceivedPacket<'a>),
    /// The sender is aware that all packets have been received, so the receiver can forget it.
    Done,
    /// This packet should be handled by someone else.
    Unhandled,
}

#[derive(Debug, Error)]
#[error("{id:?} invalid done packet id")]
pub struct InvalidDonePacketId {
    id: PacketId,
}

#[derive(Debug, Error)]
pub enum HandlePacketError {
    #[error("empty payload")]
    Empty,
    #[error("malformed packet")]
    Malformed,
    #[error(transparent)]
    Receive(#[from] PacketReceiveError),
    #[error(transparent)]
    Ack(#[from] PacketAckError),
    #[error(transparent)]
    Done(#[from] InvalidDonePacketId),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("unexpected packet kind: {0}")]
    UnexpectedKind(u8),
    #[error("packet validation error")]
    Validation,
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

pub(crate) trait SendPacket: Fn(&[u8]) -> io::Result<usize> {}
impl<F: Fn(&[u8]) -> io::Result<usize>> SendPacket for F {}

pub(crate) trait RecvPacket: Fn(&mut [u8]) -> io::Result<usize> {}
impl<F: Fn(&mut [u8]) -> io::Result<usize>> RecvPacket for F {}

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
const PART_PACKET_PAYLOAD_SIZE: NonZeroUsize = {
    match NonZeroUsize::new(PACKET_DATA_SIZE - size_of::<PacketId>() - size_of::<SeqIndex>()) {
        Some(size) => size,
        None => panic!("PACKET_BUFFER_SIZE too small to hold any payload"),
    }
};

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
