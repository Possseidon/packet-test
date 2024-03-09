use std::{
    io,
    mem::size_of,
    num::NonZeroUsize,
    time::{Duration, Instant},
};

use bitvec::vec::BitVec;
use rkyv::AlignedVec;

use super::{
    BatchSize, NonZeroBatchSize, PacketId, PacketKind, PacketSendErrorKind, SendPacket,
    SentPacketAck, SeqIndex, SeqIndexOutOfRange, SeqIndices, SeqKind, PACKET_BUFFER_SIZE,
};

/// Stores packets of any size so that they can be sent reliably.
///
/// Packets are automatically split into chunks.
#[derive(Clone, Debug)]
pub(crate) struct PacketOut {
    /// The entire payload of the packet.
    payload: AlignedVec,
    /// Which chunks have been acked so far.
    ///
    /// TODO: This is really slow for big packets, since it goes over everything repeatedly.
    /// Instead, store the unacked packets as a list of ranges.
    acked_chunks: BitVec,
    /// The total number of chunks that have been acked.
    ///
    /// The final ack is not counted to prevent a theoretical [`SeqIndex`] overflow.
    acks: SeqIndex,
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
    /// Sends a new packet with the given id and payload.
    ///
    /// - `id`: The id of the packet.
    /// - `payload`: The payload of the packet; panics if empty or over ~2TiB.
    /// - `send`: The function to send the packet; e.g. [`std::net::UdpSocket::send`].
    /// - `max_packets`: The maximum number of packets that are sent by this call.
    pub(crate) fn send(
        id: PacketId,
        payload: AlignedVec,
        send: impl SendPacket,
        initial_batch_size: NonZeroBatchSize,
    ) -> io::Result<Self> {
        assert!(!payload.is_empty());

        let (chunk_size, chunks) = Self::chunks(&payload);
        let chunk_count = chunks.len();
        let last_batch_size =
            Self::send_chunks(id, chunk_size, chunks, |_| true, send, initial_batch_size)?;
        let now = Instant::now();
        Ok(Self {
            payload,
            acked_chunks: BitVec::repeat(false, chunk_count),
            acks: 0,
            last_send: now,
            awaiting_ack_since: now,
            batch_size: initial_batch_size,
            last_batch_size,
            last_batch_acks: 0,
        })
    }

    /// Marks the given chunk as acked returning the overall state.
    pub(crate) fn ack(
        &mut self,
        seq_index: SeqIndex,
    ) -> Result<SentPacketAck, PacketSendErrorKind> {
        let seq_max = self.acked_chunks.len() - 1;
        if let Some(mut state) = self
            .acked_chunks
            .get_mut(usize::try_from(seq_index).unwrap())
        {
            if state.replace(true) {
                Ok(SentPacketAck::Pending { duplicate: true })
            } else {
                self.last_batch_acks = self.last_batch_acks.saturating_add(1);
                if usize::try_from(self.acks).unwrap() == seq_max {
                    // if all chunks were sent immediately, send_pending was never called to balance
                    self.batch_size = Self::balance_batch_size(
                        self.batch_size,
                        self.last_batch_acks,
                        self.last_batch_size,
                    );
                    Ok(SentPacketAck::Done)
                } else {
                    self.acks += 1;
                    self.awaiting_ack_since = Instant::now();
                    Ok(SentPacketAck::Pending { duplicate: false })
                }
            }
        } else {
            let seq_max = SeqIndex::try_from(seq_max).unwrap();
            Err(SeqIndexOutOfRange { seq_index, seq_max })?
        }
    }

    /// Resends parts of the packet that have not yet been acked.
    ///
    /// - `id`: The id of the packet.
    /// - `send`: The function to send the packet; e.g. [`std::net::UdpSocket::send`].
    /// - `max_packets`: The maximum number of packets that are sent by this call.
    ///
    /// Returns the number of bytes in the payload that are sent every second.
    pub(crate) fn send_pending(
        &mut self,
        id: PacketId,
        send: impl SendPacket,
        resend_delay: Duration,
    ) -> io::Result<()> {
        if self.last_send.elapsed() < resend_delay {
            return Ok(());
        }

        self.batch_size =
            Self::balance_batch_size(self.batch_size, self.last_batch_acks, self.last_batch_size);

        let (chunk_size, chunks) = Self::chunks(&self.payload);
        self.last_batch_size = Self::send_chunks(
            id,
            chunk_size,
            chunks,
            |index| !self.acked_chunks[usize::try_from(index).unwrap()],
            send,
            self.batch_size,
        )?;
        self.last_batch_acks = 0;
        self.last_send = Instant::now();
        Ok(())
    }

    /// Returns the current number of bytes of the payload that are being sent every second.
    pub(crate) fn bytes_per_sec(&self, resend_delay: Duration) -> f64 {
        self.last_batch_bytes() as f64 / resend_delay.as_secs_f64()
    }

    /// Returns the total number of payload bytes.
    pub(crate) fn total_bytes(&self) -> NonZeroUsize {
        self.payload
            .len()
            .try_into()
            .expect("payload should not be empty")
    }

    /// Returns an estimate for the number of payload bytes that have been acked.
    ///
    /// Guaranteed to be at most [`Self::payload_bytes()`], but only an estimate, since it uses the
    /// size of the first chunk multiplied by the total number of acks. That should be sufficiently
    /// accurate for most use cases though.
    pub(crate) fn acked_bytes(&self) -> usize {
        (usize::try_from(self.acks).unwrap() * self.first_chunk_size())
            .min(self.total_bytes().get())
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
        usize::try_from(self.last_batch_size.get()).unwrap() * self.first_chunk_size()
    }

    /// Returns the size of the first chunk.
    fn first_chunk_size(&self) -> usize {
        let (_, mut chunks) = Self::chunks(&self.payload);
        chunks.next().unwrap().len()
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

    /// Splits the payload into chunks.
    fn chunks(payload: &[u8]) -> (SeqKind, impl ExactSizeIterator<Item = &[u8]>) {
        for seq_kind in SeqKind::SORTED_BY_MAX_BATCH_SIZE {
            let chunks = payload.chunks(seq_kind.chunk_size());
            if chunks.len() <= seq_kind.max_batch_size() {
                return (seq_kind, chunks);
            }
        }
        panic!("packet payload too big");
    }

    /// Sends the given chunks.
    ///
    /// - `id`: The id of the packet.
    /// - `chunk_size`: The size of the chunks.
    /// - `chunks`: The entire payload as chunks.
    /// - `send_if`: A predicate that allows skipping already acked chunks.
    /// - `send`: The function to send the packet; e.g. [`std::net::UdpSocket::send`].
    /// - `max_packets`: The maximum number of packets that are sent by this call.
    fn send_chunks<'a>(
        id: PacketId,
        chunk_size: SeqKind,
        chunks: impl ExactSizeIterator<Item = &'a [u8]>,
        send_if: impl Fn(SeqIndex) -> bool,
        send: impl SendPacket,
        batch_size: NonZeroBatchSize,
    ) -> io::Result<NonZeroBatchSize> {
        let chunk_count = chunks.len();
        // count might not fit into the counter; use max index instead
        let seq_max = SeqIndex::try_from(chunk_count - 1).unwrap();
        let seq_len = chunk_size.bits() / 4;

        let kind_pos = 0;
        let uuid_pos = kind_pos + size_of::<PacketKind>();
        let seq_pos = uuid_pos + size_of::<PacketId>(); // contains index and max index
        let data_pos = seq_pos + seq_len;

        let mut sent_packets = NonZeroBatchSize::MIN;
        let mut buffer = [0; PACKET_BUFFER_SIZE];
        buffer[kind_pos] = chunk_size.packet_kind().into();
        buffer[uuid_pos..seq_pos].copy_from_slice(id.as_bytes());
        let mut seq_index: SeqIndex = 0;
        for chunk in chunks {
            if !send_if(seq_index) {
                seq_index += 1;
                continue;
            }
            let seq_index_64 = SeqIndices::from(seq_index) << chunk_size.bits();
            let seq_max_64 = SeqIndices::from(seq_max);
            let seq_indices = seq_index_64 | seq_max_64;
            let seq_range = seq_pos..data_pos;
            let seq_bytes = &seq_indices.to_le_bytes()[size_of::<SeqIndices>() - seq_len..];
            buffer[seq_range].copy_from_slice(seq_bytes);
            buffer[data_pos..data_pos + chunk.len()].copy_from_slice(chunk);
            send(&buffer)?;

            if sent_packets >= batch_size {
                break;
            }
            sent_packets = sent_packets.checked_add(1).expect("should never overflow");
            seq_index += 1;
        }

        Ok(sent_packets)
    }
}
