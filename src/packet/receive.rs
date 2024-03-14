use bitvec::vec::BitVec;
use rkyv::AlignedVec;

use super::{SeqIndex, PART_PACKET_PAYLOAD_SIZE};

/// Reassembles received packets.
#[derive(Clone, Debug, Default)]
pub(crate) struct ReassembledPacket {
    /// A buffer for the payload.
    payload: AlignedVec,
    /// Which chunks have been received so far.
    received_chunks: BitVec,
    /// The first chunk that has not been received yet.
    first_pending: SeqIndex,
    /// How many chunks have been received so far.
    ///
    /// The final packet is not counted to prevent a theoretical [`SeqIndex`] overflow.
    received_count: SeqIndex,
    /// Set to the index of the last packet once it is received.
    max_index: Option<SeqIndex>,
}

impl ReassembledPacket {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn receive(
        &mut self,
        last: bool,
        seq_index: SeqIndex,
        payload: &[u8],
    ) -> ReceivedPacket {
        if last {
            self.max_index = Some(seq_index);
        }

        let chunk_index = usize::try_from(seq_index).unwrap();
        let chunk_index_with_offset = chunk_index - usize::try_from(self.first_pending).unwrap();
        if chunk_index_with_offset >= self.received_chunks.len() {
            self.received_chunks
                .resize(chunk_index_with_offset + 1, false);
        }
        if self.received_chunks.replace(chunk_index_with_offset, true) {
            return ReceivedPacket::Duplicate;
        }
        let first_pending = self
            .received_chunks
            .first_zero()
            .unwrap_or(self.received_chunks.len());
        // round down to a byte boundary, so that the drain won't have to bitshift
        let byte_boundary = first_pending & !0xFF;
        self.received_chunks.drain(..byte_boundary);
        self.first_pending += u32::try_from(byte_boundary).unwrap();

        let payload_start = PART_PACKET_PAYLOAD_SIZE.get() * chunk_index;
        let payload_end = payload_start + payload.len();
        if payload_end > self.payload.len() {
            self.payload.resize(payload_end, 0);
        }

        self.payload[payload_start..payload_end].copy_from_slice(payload);
        if Some(self.received_count) == self.max_index {
            // don't count the last packet to prevent a theoretical overflow
            ReceivedPacket::Reassembled(&self.payload)
        } else {
            self.received_count += 1;
            ReceivedPacket::Pending
        }
    }
}

/// A part of a packet was received.
pub(crate) enum ReceivedPacket<'a> {
    /// A part of the packet was received, but the packet is not fully reassembled.
    Pending,
    /// This packet was already received and can be ignored.
    ///
    /// The packet might still be pending or it might have been reassembled already.
    Duplicate,
    /// The last part was received and the packet is fully reassembled.
    Reassembled(&'a [u8]),
}
