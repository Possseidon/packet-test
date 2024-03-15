use std::{
    mem::take,
    num::NonZeroUsize,
    sync::mpsc::{sync_channel, Receiver, SyncSender},
    thread,
};

use rkyv::{ser::Serializer, Fallible};

use super::{send::PacketBuffer, Packet, PacketId};

/// Serializes the packet, automatically switching to a thread if necessary.
///
/// 1. Serializes up to `background_serialization_threshold` packets into a [`Vec`].
/// 2. If more packets are required, spawns a detached thread and returns a channel.
///
/// The thread uses a rendezvous channel, so it only prepares a single packet without buffering
/// additional packets in advance. If the receiver is dropped, the thread will skip through the
/// remaining write calls and should finish relatively quickly.
pub(crate) fn serialize_packet<T: Packet>(
    id: PacketId,
    background_serialization_threshold: usize,
    value: T,
) -> SerializedPacket {
    let already_serialized = if let Some(background_serialization_threshold) =
        NonZeroUsize::new(background_serialization_threshold)
    {
        let mut serializer = VecPacketSerializer {
            background_serialization_threshold,
            buf: vec![PacketBuffer::new(id)],
        };
        match serializer.serialize_value(&value) {
            Ok(_) => return SerializedPacket::Vec(serializer.buf),
            Err(already_serialized) => already_serialized,
        }
    } else {
        Default::default()
    };

    let (tx, rx) = sync_channel(0);
    thread::spawn(move || {
        let skip = already_serialized.len();
        for buf in already_serialized {
            if tx.send(buf).is_err() {
                return;
            }
        }

        ChannelPacketSerializer {
            skip,
            buf: PacketBuffer::new(id),
            tx,
        }
        .serialize_value(&value)
        .ok();
    });
    SerializedPacket::Channel(rx)
}

pub(crate) enum SerializedPacket {
    Vec(Vec<PacketBuffer>),
    Channel(Receiver<PacketBuffer>),
}

pub struct VecPacketSerializer {
    background_serialization_threshold: NonZeroUsize,
    buf: Vec<PacketBuffer>,
}

impl Fallible for VecPacketSerializer {
    type Error = Vec<PacketBuffer>;
}

impl Serializer for VecPacketSerializer {
    fn pos(&self) -> usize {
        0
    }

    fn write(&mut self, mut bytes: &[u8]) -> Result<(), Self::Error> {
        while !bytes.is_empty() {
            let at_limit = self.buf.len() == self.background_serialization_threshold.get();
            let last = self.buf.last_mut().expect("should not be empty");
            let before = bytes.len();
            bytes = last.append(bytes);
            let after = bytes.len();
            if after == before {
                if at_limit {
                    return Err(take(&mut self.buf));
                }
                let packet = last.new_next();
                self.buf.push(packet);
            }
        }
        Ok(())
    }
}

pub struct ChannelPacketSerializer {
    skip: usize,
    buf: PacketBuffer,
    tx: SyncSender<PacketBuffer>,
}

impl Fallible for ChannelPacketSerializer {
    type Error = ReceiverDropped;
}

struct ReceiverDropped;

impl Serializer for ChannelPacketSerializer {
    fn pos(&self) -> usize {
        0
    }

    fn write(&mut self, mut bytes: &[u8]) -> Result<(), Self::Error> {
        while !bytes.is_empty() {
            let before = bytes.len();
            bytes = if self.skip > 0 {
                self.buf.append(bytes)
            } else {
                self.buf.skip(bytes)
            };
            let after = bytes.len();
            if after == before {
                if self.skip > 0 {
                    self.skip -= 1;
                } else if self.tx.send(self.buf.copy()).is_err() {
                    return Err(ReceiverDropped);
                }
                self.buf.next();
            }
        }
        Ok(())
    }
}

impl Drop for ChannelPacketSerializer {
    fn drop(&mut self) {
        assert!(self.skip == 0);
        self.buf.mark_last();
        self.tx.send(self.buf.copy()).ok();
    }
}
