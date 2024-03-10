use std::{
    sync::mpsc::{sync_channel, Receiver, SyncSender},
    thread::{self, JoinHandle},
};

use rkyv::{ser::Serializer, Fallible, Infallible};

use super::{send::PacketBuffer, Packet, PacketId, PacketKind, SeqIndex, PACKET_BUFFER_SIZE};

/// Spawns a thread that serializes the given
pub(crate) fn spawn_serialize_packet<T: Packet>(
    id: PacketId,
    value: T,
) -> (Receiver<PacketBuffer>, JoinHandle<()>) {
    let (tx, rx) = sync_channel(0);
    (
        rx,
        thread::spawn(move || {
            ChannelSerializer {
                id,
                seq_index: 0,
                buffer: PacketBuffer::default(),
                tx,
            }
            .serialize_value(&value)
            .unwrap();
        }),
    )
}

pub(crate) struct ChannelSerializer {
    id: PacketId,
    seq_index: SeqIndex,
    buffer: PacketBuffer,
    tx: SyncSender<PacketBuffer>,
}

impl Fallible for ChannelSerializer {
    type Error = Infallible;
}

impl Serializer for ChannelSerializer {
    fn pos(&self) -> usize {
        // TODO: This might need to return the proper value, note sure
        0
    }

    fn write(&mut self, mut bytes: &[u8]) -> Result<(), Self::Error> {
        while !bytes.is_empty() {
            if self.buffer.is_empty() {
                self.buffer.append(&[PacketKind::Part.into()]);
                self.buffer.append(self.id.as_bytes());
                self.buffer.append(&self.seq_index.to_le_bytes());
                // seq_index won't be used after wrapping
                self.seq_index = self.seq_index.wrapping_add(1);
            }

            let end = (self.buffer.len() + bytes.len()).min(PACKET_BUFFER_SIZE);
            let count = end - self.buffer.len();
            self.buffer.append(&bytes[..count]);

            bytes = &bytes[count..];

            if self.buffer.len() == PACKET_BUFFER_SIZE {
                self.tx.send(self.buffer.clone()).unwrap();
                self.buffer.clear();
            }
        }
        Ok(())
    }
}

impl Drop for ChannelSerializer {
    fn drop(&mut self) {
        if !self.buffer.is_empty() {
            self.tx.send(self.buffer.clone()).unwrap();
        }
    }
}
