use std::num::NonZeroUsize;

use bitvec::vec::BitVec;
use uuid::Uuid;

use super::{PacketReceiveErrorKind, ReceivedPacketAck, SeqIndex, SeqIndexOutOfRange, SeqKind};

/// Reassembles received packets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReassembledPacket {
    /// A buffer for the payload.
    ///
    /// Slightly bigger than necessary, since only the number of chunks is known.
    payload: Box<[u8]>,
    /// Set to the proper length of the payload once the last packet is received.
    payload_len: Option<NonZeroUsize>,
    /// Which chunks have been received so far.
    received_chunks: BitVec,
    /// How many chunks have been received so far.
    ///
    /// The final packet is not counted to prevent a theoretical [`SeqIndex`] overflow.
    received_count: SeqIndex,
}

impl ReassembledPacket {
    pub(crate) fn new(seq_kind: SeqKind, seq_max: SeqIndex) -> Self {
        let len = usize::try_from(seq_max).unwrap() + 1;
        Self {
            payload: vec![0; len * seq_kind.chunk_size()].into_boxed_slice(),
            payload_len: None,
            received_chunks: BitVec::repeat(false, usize::try_from(seq_max).unwrap()),
            received_count: 0,
        }
    }

    pub(crate) fn receive(
        &mut self,
        id: Uuid,
        seq_kind: SeqKind,
        seq_index: SeqIndex,
        payload: &[u8],
    ) -> Result<ReceivedPacketAck, PacketReceiveErrorKind> {
        let chunk_count = self.received_chunks.len();
        let seq_max = SeqIndex::try_from(chunk_count - 1).unwrap();
        let index = usize::try_from(seq_index).unwrap();

        if index >= self.received_chunks.len() {
            return Err(PacketReceiveErrorKind::SeqIndexOutOfRange(
                SeqIndexOutOfRange { seq_index, seq_max },
            ));
        }

        let offset = index * seq_kind.chunk_size();
        if !self.received_chunks.replace(index, true) {
            if seq_index == seq_max {
                self.payload_len = Some(
                    NonZeroUsize::new((chunk_count - 1) * seq_kind.chunk_size() + payload.len())
                        .unwrap(),
                );
            }

            self.payload[offset..offset + payload.len()].copy_from_slice(payload);
            if usize::try_from(self.received_count).unwrap() == self.received_chunks.len() - 1 {
                Ok(ReceivedPacketAck::Done(&self.payload))
            } else {
                self.received_count += 1;
                Ok(ReceivedPacketAck::Pending { duplicate: false })
            }
        } else {
            Ok(ReceivedPacketAck::Pending { duplicate: true })
        }
    }
}
