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
    /// Sent out by the server upon the client requesting its version by sending an empty packet.
    ///
    /// Used to determine if the client and server are compatible in terms of packets.
    ///
    /// It is highly recommended for the layout of this packet to either not change or at least be
    /// backward/forward compatible. By doing so, there will be no false positives when checking
    /// compatibility and it also allows for nicer error messages in the case of incompatibility.
    ///
    /// I.e. if the layout changes, the client can no longer know the exact version of the server
    /// or (while unlikely) it might even interpret an incompatible version packet as being
    /// compatible to its own version.
    type Version: VersionPacket;
    /// Contains additional information in case of a version mismatch.
    type CompatibilityError: std::error::Error;

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
    type Version = ();
    type CompatibilityError = Infallible;

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
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, Archive, Serialize)]
#[archive(check_bytes)]
pub enum ServerConnectionPacket<P: Packets> {
    Status(P::Status),
    Accept(P::Accept),
    Reject(P::Reject),
    Kick(P::Kick),
    User(P::Server),
}

/// Client packets for establishing a server connection.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, Archive, Serialize)]
#[archive(check_bytes)]
pub enum ClientConnectionPacket<P: Packets> {
    Query(P::Query),
    Connect(P::Connect),
    Disconnect(P::Disconnect),
    User(P::Client),
}

pub(crate) type ServerPacketBuffers<P> =
    PacketBuffers<ServerConnectionPacket<P>, ClientConnectionPacket<P>>;

pub(crate) type ClientPacketBuffers<P> =
    PacketBuffers<ClientConnectionPacket<P>, ServerConnectionPacket<P>>;

#[derive(Debug, Error)]
#[error("malformed version packet")]
pub struct MalformedVersion;

pub trait VersionPacket: Sized {
    /// Writes version information to the given buffer.
    ///
    /// Returns the number of bytes written, which must be at most [`PACKET_DATA_SIZE`].
    fn write(&self, buf: &mut [u8]) -> usize;

    /// Tries to read version information from the given buffer.
    fn read(buf: &[u8]) -> Result<Self, MalformedVersion>;
}

/// Does not contain any version information but only accepts empty packets in response as well.
impl VersionPacket for () {
    fn write(&self, _buf: &mut [u8]) -> usize {
        0
    }

    fn read(buf: &[u8]) -> Result<Self, MalformedVersion> {
        if buf.is_empty() {
            Ok(())
        } else {
            Err(MalformedVersion)
        }
    }
}
/// Packets for things that constantly change, which means it's okay if some are dropped.
///
/// These packets must fit into a single datagram.
#[allow(clippy::len_without_is_empty)] // len is always > 0
pub trait UpdatePacket: Sized {
    type ReadError: std::error::Error;

    /// The size of the packet, which must be at most [`PACKET_BUFFER_SIZE`].
    ///
    /// Used as an upfront check to see if the packet might fit in the remaining buffer.
    fn len(&self) -> NonZeroBufferIndex;

    /// Writes exactly [`UpdatePacket::len()`] bytes to the given buffer.
    ///
    /// Panics if the buffer is too small.
    fn write(&self, buf: &mut [u8]);

    /// Tries to read a packet from the given buffer.
    ///
    /// The returned value can be used to get the number of read bytes via [`UpdatePacket::len()`].
    fn read(buf: &[u8]) -> Result<Self, Self::ReadError>;
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum NoPacket {}

impl Archive for NoPacket {
    type Archived = ArchivedNoPacket;
    type Resolver = ();

    unsafe fn resolve(&self, _pos: usize, _resolver: Self::Resolver, _out: *mut Self::Archived) {
        match *self {}
    }
}

impl<S: Fallible + ?Sized> Serialize<S> for NoPacket {
    fn serialize(&self, _serializer: &mut S) -> Result<Self::Resolver, S::Error> {
        match *self {}
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum ArchivedNoPacket {}

impl<C: ?Sized> CheckBytes<C> for ArchivedNoPacket {
    type Error = Infallible;

    unsafe fn check_bytes<'a>(
        _value: *const Self,
        _context: &mut C,
    ) -> Result<&'a Self, Self::Error> {
        match *_value {}
    }
}

#[derive(Debug, Error)]
#[error("there are no update packets")]
pub struct NoUpdateError;

impl UpdatePacket for NoPacket {
    type ReadError = NoUpdateError;

    fn len(&self) -> NonZeroBufferIndex {
        match *self {}
    }

    fn write(&self, _buf: &mut [u8]) {
        match *self {}
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
        socket: impl NonBlocking,
    ) -> io::Result<PacketId> {
        self.send_buffer
            .send(packet, background_serialization_threshold, socket)
    }

    pub(crate) fn update(
        &mut self,
        resend_delay: Duration,
        max_missed_acks: Option<usize>,
        receive_timeout: Option<Duration>,
        socket: impl NonBlocking,
    ) -> io::Result<()> {
        self.timeout_packets(max_missed_acks, receive_timeout);
        self.send_pending(resend_delay, socket)?;
        Ok(())
    }

    pub(crate) fn handle<'a>(
        &'a mut self,
        buf: &'a [u8],
        socket: impl NonBlocking,
    ) -> Result<PacketHandling<'a>, HandlePacketError> {
        if buf.is_empty() {
            return Err(HandlePacketError::Empty);
        }

        match self.receive_buffer.handle(buf, socket)? {
            ReceivedPacketHandling::Ack(received) => return Ok(PacketHandling::Received(received)),
            ReceivedPacketHandling::Done { known_packet } => {
                return Ok(PacketHandling::Done { known_packet })
            }
            ReceivedPacketHandling::Unhandled => {}
        }

        match self.send_buffer.handle(buf)? {
            SentPacketHandling::Ack { duplicate } => return Ok(PacketHandling::Ack { duplicate }),
            SentPacketHandling::Unhandled => {}
        }

        // TODO: Handle update packets

        Err(HandlePacketError::UnexpectedKind(buf[0]))
    }

    fn send_pending(&mut self, resend_delay: Duration, socket: impl NonBlocking) -> io::Result<()> {
        self.send_buffer.send_pending(resend_delay, socket)
    }

    fn timeout_packets(
        &mut self,
        max_missed_acks: Option<usize>,
        receive_timeout: Option<Duration>,
    ) {
        if let Some(max_missed_acks) = max_missed_acks {
            self.send_buffer.timeout_packets(max_missed_acks);
        }
        if let Some(receive_timeout) = receive_timeout {
            self.receive_buffer.timeout_packets(receive_timeout);
        }
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
    /// The sender will stop sending parts for this packet.
    Done { known_packet: bool },
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
                PacketKind::Version
                | PacketKind::Ping
                | PacketKind::Part
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
        socket: impl NonBlocking,
    ) -> io::Result<PacketId> {
        let id = PacketId::new();
        self.packets.insert(
            id,
            PacketOut::send(
                serialize_packet(id, background_serialization_threshold, packet),
                socket,
                self.batch_size,
            )?,
        );
        Ok(id)
    }

    /// Sends out still pending packet parts.
    fn send_pending(&mut self, resend_delay: Duration, socket: impl NonBlocking) -> io::Result<()> {
        let mut batch_size: usize = 0;
        let mut count: usize = 0;
        let mut result = Ok(());
        self.packets.retain(|id, packet| {
            batch_size = batch_size.saturating_add(usize::from(packet.batch_size().get()));
            count += 1;
            if result.is_err() {
                return true;
            }
            match packet.send_pending(*id, resend_delay, socket) {
                Ok(done) => !done,
                Err(error) => {
                    result = Err(error);
                    true
                }
            }
        });
        if count != 0 {
            let batch_size = BatchSize::try_from(batch_size / count).unwrap_or(BatchSize::MAX);
            self.batch_size = NonZeroBatchSize::new(batch_size).unwrap_or(NonZeroBatchSize::MIN);
        }

        result
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

    /// Forget packets that did not get acks in a long time.
    fn timeout_packets(&mut self, max_missed_acks: usize) {
        self.packets
            .retain(|_, packet| packet.missed_acks() <= max_missed_acks);
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
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
        socket: impl NonBlocking,
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
                        socket,
                    )?))
                }
                PacketKind::Done => Ok(ReceivedPacketHandling::Done {
                    known_packet: self.done(id()?),
                }),
                PacketKind::Version
                | PacketKind::Ping
                | PacketKind::Ack
                | PacketKind::FirstUpdate => Ok(ReceivedPacketHandling::Unhandled),
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
        socket: impl NonBlocking,
    ) -> Result<ReceivedPacket<'a>, PacketReceiveError> {
        if last && seq_index == 0 {
            match self.packets.entry(id) {
                Entry::Vacant(entry) => {
                    entry.insert(PacketIn {
                        last_receive: Instant::now(),
                        packet: None,
                    });
                    ack(id, seq_index, socket)?;
                    Ok(ReceivedPacket::Reassembled(payload))
                }
                Entry::Occupied(entry) => {
                    if entry.get().packet.is_none() {
                        ack(id, seq_index, socket)?;
                        Ok(ReceivedPacket::Duplicate)
                    } else {
                        Err(PacketReceiveError {
                            id,
                            kind: PacketReceiveErrorKind::Mismatch,
                        })
                    }
                }
            }
        } else {
            let packet = self
                .packets
                .entry(id)
                .and_modify(|packet| packet.last_receive = Instant::now())
                .or_insert_with(|| PacketIn {
                    last_receive: Instant::now(),
                    packet: Some(ReassembledPacket::new()),
                });
            if let Some(packet) = &mut packet.packet {
                let received_packet = packet.receive(last, seq_index, payload);
                ack(id, seq_index, socket)?;
                Ok(received_packet)
            } else {
                Err(PacketReceiveError {
                    id,
                    kind: PacketReceiveErrorKind::Mismatch,
                })
            }
        }
    }

    /// The sender will no longer send any parts for this packet.
    ///
    /// Either because all parts were acked or because the packet was canceled.
    ///
    /// Returns `true` if the client was aware of this packet.
    fn done(&mut self, id: PacketId) -> bool {
        self.packets.remove(&id).is_some()
    }

    fn stats(&self, _id: PacketId) -> PacketInStats {
        todo!()
    }

    /// Forget packets that did not receive data in a long time.
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
    Done { known_packet: bool },
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
#[error("the packet sequence index {seq_index} got acked before it was sent")]
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

/// A non-blocking socket that can send data.
pub(crate) trait NonBlocking: Copy {
    /// Returns `false` if it wasn't sent because the socket would block.
    ///
    /// This allows propagating errors using `?`, since "would block" is no longer treated as one.
    fn send(self, buf: &[u8]) -> io::Result<bool>;
}

#[derive(
    Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, IntoPrimitive, TryFromPrimitive,
)]
#[repr(u8)]
pub(crate) enum PacketKind {
    /// Used to check for compatibility between client and server.
    ///
    /// - A client sends out an empty packet to a server to request its version.
    /// - A server, in turn, responds with a [`Packets::Version`] packet.
    Version,
    /// Sent out periodically by the server, expecting the client to respond to it.
    ///
    /// - If a server doesn't get a pong for a long time, that client gets timed out.
    /// - If a client doesn't receive a ping from the server, it also times out.
    ///
    /// Pongs also use [`PacketKind::Ping`]. It is simply considered a "pong" when sent by a client.
    ///
    /// Every new ping packet also contains a freshly generated UUID that must be provided in the
    /// pong packet to prevent clients from faking their ping times by sending pongs before a ping.
    Ping,
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
    /// Not a packet kind, but instead marks the start of the first update packet.
    ///
    /// - [`Packets::ServerUpdate`] when sent by a server to a client
    /// - [`Packets::ClientUpdate`] when sent by a client to a server
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

fn ack(
    id: PacketId,
    seq_index: SeqIndex,
    socket: impl NonBlocking,
) -> Result<(), PacketReceiveError> {
    let mut ack = [0; 17 + size_of::<SeqIndex>()];
    ack[0] = PacketKind::Ack as u8;
    ack[1..17].copy_from_slice(id.as_bytes());
    ack[17..17 + size_of::<SeqIndex>()].copy_from_slice(&seq_index.to_le_bytes());
    socket.send(&ack).map_err(|error| PacketReceiveError {
        id,
        kind: error.into(),
    })?;
    // Ignore if it would block; it's more likely that a packet gets dropped, which results in the
    // same situation, except that it's not possible to detect if a packet was dropped.
    // Currently acks are only sent out in response to a "receive" packet, but blocking acks would
    // have to be retried periodically, which complicates things quite a bit with no real benefit.
    Ok(())
}
