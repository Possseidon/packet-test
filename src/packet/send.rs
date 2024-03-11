use std::{
    collections::VecDeque,
    io,
    iter::zip,
    mem::{replace, size_of},
    ops::{Deref, DerefMut},
    sync::mpsc::Receiver,
    time::{Duration, Instant},
};

use rkyv::AlignedBytes;

use super::{
    serialize::SerializedPacket, BatchSize, BufferIndex, NonZeroBatchSize, PacketId, PacketKind,
    SendPacket, SeqIndex, SeqIndexAckedBeforeSent, PACKET_BUFFER_SIZE,
};

/// Stores packets of any size so that they can be sent reliably.
///
/// Packets are automatically split into chunks.
#[derive(Debug)]
pub(crate) struct PacketOut {
    /// Contains the packets to be sent.
    packet_queue: PacketQueue,
    /// The last time a batch of packets was sent.
    last_send: Instant,
    /// Updated whenever and ack is received; used to time out old packets.
    awaiting_ack_since: Instant,
    /// Up to how many packets are sent at once.
    batch_size: NonZeroBatchSize,
    /// How many packets were sent in the last batch.
    last_batch_size: NonZeroBatchSize,
    /// How many acks have been received for the last batch.
    last_batch_acks: BatchSize,
}

impl PacketOut {
    pub(crate) fn send(
        packet: SerializedPacket,
        send: impl SendPacket,
        initial_batch_size: NonZeroBatchSize,
    ) -> io::Result<Self> {
        let mut packet_queue = PacketQueue::new(packet);
        let last_batch_size =
            NonZeroBatchSize::new(packet_queue.send_unacked(initial_batch_size, send)?)
                .expect("packet should not be empty");
        let now = Instant::now();
        Ok(Self {
            packet_queue,
            last_send: now,
            awaiting_ack_since: now,
            batch_size: initial_batch_size,
            last_batch_size,
            last_batch_acks: 0,
        })
    }

    /// Marks the given packet as acked and returns true if it was already acked previously.
    pub(crate) fn ack(&mut self, seq_index: SeqIndex) -> Result<bool, SeqIndexAckedBeforeSent> {
        self.awaiting_ack_since = Instant::now();

        if self.packet_queue.ack(seq_index)? {
            return Ok(true);
        }

        self.last_batch_acks = self.last_batch_acks.saturating_add(1);
        Ok(false)
    }

    /// Resends parts of the packet that have not yet been acked.
    ///
    /// - `id`: The id of the packet.
    /// - `send`: The function to send the packet; e.g. [`std::net::UdpSocket::send`].
    pub(crate) fn send_pending(
        &mut self,
        id: PacketId,
        resend_delay: Duration,
        send: impl SendPacket,
    ) -> io::Result<()> {
        if self.last_send.elapsed() < resend_delay {
            return Ok(());
        }

        self.batch_size =
            Self::balance_batch_size(self.batch_size, self.last_batch_acks, self.last_batch_size);

        if let Some(count) =
            NonZeroBatchSize::new(self.packet_queue.send_unacked(self.batch_size, &send)?)
        {
            self.last_batch_size = count;
        } else {
            done(id, send)?;
        }
        self.last_batch_acks = 0;
        self.last_send = Instant::now();
        Ok(())
    }

    /// Returns the current number of bytes of the payload that are being sent every second.
    pub(crate) fn bytes_per_sec(&self, resend_delay: Duration) -> f64 {
        self.last_batch_bytes() as f64 / resend_delay.as_secs_f64()
    }

    /// Returns an estimate for the number of payload bytes that have been acked.
    pub(crate) fn acked_bytes(&self) -> usize {
        // TODO: improve estimation; PACKET_BUFFER_SIZE is too large
        self.packet_queue.first_unacked * PACKET_BUFFER_SIZE
    }

    /// Since when the sender is waiting for an ack.
    pub(crate) fn awaiting_ack_since(&self) -> Instant {
        self.awaiting_ack_since
    }

    /// Up to this many packets are being sent at once.
    ///
    /// This value is balanced automatically.
    pub(crate) fn batch_size(&self) -> NonZeroBatchSize {
        self.batch_size
    }

    /// Returns an estimate for the current number of bytes of the payload, sent by the last batch.
    ///
    /// This is only an estimate and might even be bigger than [`Self::payload_bytes()`], but since
    /// this is intended to be used for measuring throughput, it should not matter all that much.
    fn last_batch_bytes(&self) -> usize {
        // TODO: improve estimation, PACKET_BUFFER_SIZE is too large
        usize::from(self.last_batch_size.get()) * PACKET_BUFFER_SIZE
    }

    /// Adjusts the batch size based on the number of acked chunks.
    fn balance_batch_size(
        old_batch_size: NonZeroBatchSize,
        last_batch_acks: BatchSize,
        last_batch_size: NonZeroBatchSize,
    ) -> NonZeroBatchSize {
        // acked_count might be greater if the client is pretending to receive packets that were
        // never sent; since the sender does not remember which packets it sent, just ignore it
        if last_batch_acks >= last_batch_size.get() {
            // all packets of the last batch were acked, try up to twice as much next time
            //
            // using last_batch_size instead of old_batch_size prevents the batch size from growing
            // indefinitely if there are a lot of tiny packets that all get acked
            let doubled_last_batch_size =
                last_batch_size.saturating_mul(NonZeroBatchSize::new(2).unwrap());
            // a batch can contain less than old_batch_size packets, never lower it
            old_batch_size.max(doubled_last_batch_size)
        } else {
            // not all packets of the last batch were acked, cut the batch size in half
            //
            // this uses old_batch_size instead of last_batch_size, since the last batch might have
            // dropped some packets despite being e.g. just a single packet, which would lower the
            // batch size down all the way to one unnecessarily
            let halfed = old_batch_size.get() / 2;
            // a batch shoould always contain at least one packet
            NonZeroBatchSize::new(halfed).unwrap_or(NonZeroBatchSize::MIN)
        }
    }
}

#[derive(Debug)]
struct PacketQueue {
    /// A buffer with packets that have not yet been acked.
    ///
    /// Any front packets that are acked can be removed while incrementing [`Self::first_unacked`].
    packets: VecDeque<PacketBuffer>,
    /// The index of the first unacked packet, i.e. the offset to add onto [`Self::packets`].
    first_unacked: usize,
    /// A channel that provides additional packet buffers to chain onto [`Self::packets`].
    ///
    /// These packets are generated on a separate thread.
    rx: Option<Receiver<PacketBuffer>>,
}

impl PacketQueue {
    fn new(packet: SerializedPacket) -> Self {
        match packet {
            SerializedPacket::Vec(packets) => Self {
                packets: packets.into(),
                first_unacked: 0,
                rx: None,
            },
            SerializedPacket::Channel(rx) => Self {
                packets: Default::default(),
                first_unacked: 0,
                rx: Some(rx),
            },
        }
    }

    /// Sends up to `batch_size` packets and returns the number of packets that were actually sent.
    fn send_unacked(
        &mut self,
        batch_size: NonZeroBatchSize,
        send: impl SendPacket,
    ) -> io::Result<BatchSize> {
        let mut count = 0;
        println!("Sending unacked packets...");
        for packet in self.unacked(batch_size.get().into()) {
            println!("  {} bytes", packet.len());
            send(packet)?;
            count += 1;
        }
        Ok(count)
    }

    fn unacked(&mut self, count: usize) -> impl Iterator<Item = &[u8]> {
        if let Some(rx) = &self.rx {
            let existing = self
                .packets
                .iter()
                .filter(|packet| !packet.acked)
                .take(count)
                .count();
            self.packets
                .extend(zip(existing..count, rx).map(|(_, packet)| packet));
        }
        self.packets
            .iter()
            .filter(|packet| !packet.acked)
            .map(|packet| packet.as_ref())
            .take(count)
    }

    /// Marks the packet as acked and returns true if it was already acked previously.
    fn ack(&mut self, seq_index: SeqIndex) -> Result<bool, SeqIndexAckedBeforeSent> {
        let acked = &mut self
            .packets
            .get_mut(usize::try_from(seq_index).unwrap())
            .ok_or(SeqIndexAckedBeforeSent { seq_index })?
            .acked;
        if replace(acked, true) {
            return Ok(true);
        }
        let first_unacked = self
            .packets
            .iter()
            .position(|packet| !packet.acked)
            .unwrap_or(self.packets.len());
        self.packets.drain(..first_unacked);
        self.first_unacked += first_unacked;

        Ok(false)
    }
}

pub(crate) struct PacketBuffer {
    acked: bool,
    len: BufferIndex,
    data: AlignedBytes<PACKET_BUFFER_SIZE>,
}

impl PacketBuffer {
    const HEADER_LEN: usize =
        size_of::<PacketKind>() + size_of::<PacketId>() + size_of::<SeqIndex>();

    pub(crate) fn new(id: PacketId) -> Self {
        let mut result = Self {
            acked: false,
            len: 0,
            data: Default::default(),
        };
        result.append(&[PacketKind::Part.into()]);
        result.append(id.as_bytes());
        result.append(&(0 as SeqIndex).to_le_bytes());
        result.len = BufferIndex::try_from(
            size_of::<PacketKind>() + size_of::<PacketId>() + size_of::<SeqIndex>(),
        )
        .unwrap();
        result
    }

    pub(crate) fn copy(&self) -> Self {
        let mut result = Self {
            acked: false,
            len: self.len,
            data: Default::default(),
        };
        let len = usize::from(self.len);
        result.data[..len].copy_from_slice(&self.data[..len]);
        result
    }

    pub(crate) fn next(&mut self) {
        assert_eq!(usize::from(self.len), PACKET_BUFFER_SIZE);
        self.len = BufferIndex::try_from(Self::HEADER_LEN).unwrap();
        self.inc_seq_index();
    }

    pub(crate) fn mark_last(&mut self) {
        self.data[0] = PacketKind::LastPart.into();
    }

    pub(crate) fn append<'a>(&mut self, data: &'a [u8]) -> &'a [u8] {
        let space = PACKET_BUFFER_SIZE - self.len();
        let len = data.len().min(space);
        let end = self.len();
        let new_end = end + len;
        self.data[end..new_end].copy_from_slice(&data[..len]);
        self.len = BufferIndex::try_from(new_end).unwrap();
        &data[len..]
    }

    pub(crate) fn new_next(&self) -> Self {
        let mut result = Self {
            acked: false,
            len: BufferIndex::try_from(Self::HEADER_LEN).unwrap(),
            data: Default::default(),
        };
        result.data[..Self::HEADER_LEN].copy_from_slice(&self.data[..Self::HEADER_LEN]);
        result.inc_seq_index();
        result
    }

    fn inc_seq_index(&mut self) {
        let start = size_of::<PacketKind>() + size_of::<PacketId>();
        let range = start..Self::HEADER_LEN;
        let next_seq_index =
            SeqIndex::from_le_bytes(self.data[range.clone()].try_into().unwrap()) + 1;
        self.data[range].copy_from_slice(&SeqIndex::to_le_bytes(next_seq_index));
    }
}

impl std::fmt::Debug for PacketBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PacketBuffer")
            .field("acked", &self.acked)
            .field("data", &self.deref())
            .finish()
    }
}

impl Deref for PacketBuffer {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.data[0..self.len.into()]
    }
}

impl DerefMut for PacketBuffer {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.data[0..self.len.into()]
    }
}

impl<T> AsRef<T> for PacketBuffer
where
    T: ?Sized,
    <PacketBuffer as Deref>::Target: AsRef<T>,
{
    fn as_ref(&self) -> &T {
        self.deref().as_ref()
    }
}

impl<T> AsMut<T> for PacketBuffer
where
    <PacketBuffer as Deref>::Target: AsMut<T>,
{
    fn as_mut(&mut self) -> &mut T {
        self.deref_mut().as_mut()
    }
}

fn done(id: PacketId, send: impl SendPacket) -> io::Result<()> {
    let mut done = [0; 17];
    done[0] = PacketKind::Done as u8;
    done[1..17].copy_from_slice(id.as_bytes());
    send(&done)?;
    Ok(())
}
