use std::{
    convert::Infallible,
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
    if let Some(background_serialization_threshold) =
        NonZeroUsize::new(background_serialization_threshold)
    {
        let mut serializer = VecPacketSerializer {
            background_serialization_threshold,
            buf: vec![PacketBuffer::new(id)],
        };
        if serializer.serialize_value(&value).is_ok() {
            return SerializedPacket::Vec(serializer.buf);
        }
    }

    println!("Using a channel for serialization...");
    let (tx, rx) = sync_channel(0);
    thread::spawn(move || {
        ChannelPacketSerializer {
            buf: PacketBuffer::new(id),
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

pub struct VecPacketSerializer {
    background_serialization_threshold: NonZeroUsize,
    buf: Vec<PacketBuffer>,
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
            let at_limit = self.buf.len() == self.background_serialization_threshold.get();
            let last = self.buf.last_mut().expect("should not be empty");
            let before = bytes.len();
            bytes = last.append(bytes);
            let after = bytes.len();
            if after == before {
                if at_limit {
                    return Err(());
                }
                let packet = last.new_next();
                self.buf.push(packet);
            }
        }
        Ok(())
    }
}

pub struct ChannelPacketSerializer {
    buf: PacketBuffer,
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
            bytes = self.buf.append(bytes);
            let after = bytes.len();
            if after == before {
                if tx.send(self.buf.copy()).is_err() {
                    self.tx = None;
                    return Ok(());
                }
                self.buf.next();
            }
        }
        Ok(())
    }
}

impl Drop for ChannelPacketSerializer {
    fn drop(&mut self) {
        if let Some(tx) = &self.tx {
            assert!(!self.buf.is_empty());
            self.buf.mark_last();
            tx.send(self.buf.copy()).unwrap();
        }
    }
}
