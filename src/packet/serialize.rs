use std::{
    convert::Infallible,
    sync::mpsc::{sync_channel, Receiver, SyncSender},
    thread,
};

use rkyv::{ser::Serializer, Fallible};

use super::{send::PacketBuffer, Packet, PacketId};

/// Serializes the packet, automatically switching to a thread if necessary.
///
/// 1. Serializes up to `max_direct` packets into a [`Vec`].
/// 2. If more packets are required, spawns a detached thread and returns a channel.
///
/// The thread uses a rendezvous channel, so it only prepares a single packet without buffering
/// additional packets in advance. If the receiver is dropped, the thread will skip through the
/// remaining write calls and should finish relatively quickly.
pub(crate) fn serialize_packet<T: Packet>(
    id: PacketId,
    max_direct: usize,
    value: T,
) -> SerializedPacket {
    let mut serializer = VecPacketSerializer {
        max_packets: max_direct,
        buffer: vec![PacketBuffer::new(id)],
    };
    if serializer.serialize_value(&value).is_ok() {
        return SerializedPacket::Vec(serializer.buffer);
    }

    let (tx, rx) = sync_channel(0);
    thread::spawn(move || {
        ChannelPacketSerializer {
            buffer: PacketBuffer::new(id),
            tx: Some(tx),
        }
        .serialize_value(&value)
        .expect("should be infallible");
    });
    SerializedPacket::Channel(rx)
}

pub(crate) enum SerializedPacket {
    Vec(Vec<PacketBuffer>),
    Channel(Receiver<PacketBuffer>),
}

#[derive(Default)]
pub struct VecPacketSerializer {
    max_packets: usize,
    buffer: Vec<PacketBuffer>,
}

impl Fallible for VecPacketSerializer {
    type Error = ();
}

impl Serializer for VecPacketSerializer {
    fn pos(&self) -> usize {
        0
    }

    fn write(&mut self, mut bytes: &[u8]) -> Result<(), Self::Error> {
        while !bytes.is_empty() {
            let at_limit = self.buffer.len() == self.max_packets;
            let last = self.buffer.last_mut().expect("should not be empty");
            let before = bytes.len();
            bytes = last.append(bytes);
            let after = bytes.len();
            if after == before {
                if at_limit {
                    return Err(());
                }
                let packet = last.new_next();
                self.buffer.push(packet);
            }
        }
        Ok(())
    }
}

pub struct ChannelPacketSerializer {
    buffer: PacketBuffer,
    tx: Option<SyncSender<PacketBuffer>>,
}

impl Fallible for ChannelPacketSerializer {
    type Error = Infallible;
}

impl Serializer for ChannelPacketSerializer {
    fn pos(&self) -> usize {
        0
    }

    fn write(&mut self, mut bytes: &[u8]) -> Result<(), Self::Error> {
        let Some(tx) = &self.tx else {
            // just skip through everything if the channel is closed
            return Ok(());
        };
        while !bytes.is_empty() {
            let before = bytes.len();
            bytes = self.buffer.append(bytes);
            let after = bytes.len();
            if after == before {
                if tx.send(self.buffer.copy()).is_err() {
                    self.tx = None;
                    return Ok(());
                }
                self.buffer.next();
            }
        }
        Ok(())
    }
}

impl Drop for ChannelPacketSerializer {
    fn drop(&mut self) {
        if let Some(tx) = &self.tx {
            assert!(!self.buffer.is_empty());
            self.buffer.mark_last();
            tx.send(self.buffer.copy()).unwrap();
        }
    }
}
