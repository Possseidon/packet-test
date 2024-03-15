pub mod client;
pub mod server;

use std::{
    error::Error,
    io::{self, ErrorKind},
    net::{SocketAddr, UdpSocket},
};

use thiserror::Error;

use crate::packet::{HandlePacketError, NonBlocking};

/// Allows listening for the underlying packet parts as well as deal with errors.
pub trait ConnectionHandler {
    /// An update for an outgoing or incoming packet was received.
    ///
    /// The main use for this is to gather statistics about a connection (how man total packets were
    /// sent/received, how many were duplicates, etc...).
    ///
    /// If this information is not needed, this can safely be ignored.
    fn raw_packet(&mut self, packet: RawPacket) {
        _ = packet;
    }

    /// Reports errors like packet validation failure, networking IO errors, etc...
    ///
    /// While these can be safely ignored, it probably doesn't hurt to at the very least log them.
    fn error(&mut self, error: HandlerError) {
        _ = error;
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum RawPacket {
    /// A part of a packet was received.
    Part {
        /// The sender of this packet part.
        sender_addr: SocketAddr,
        /// Information about the received part.
        info: PartInfo,
    },
    /// The sender will no longer send updates for this packet.
    ///
    /// Either because the packet was fully acked by the receiver, or because the sender is no
    /// longer interested in the client receiving this packet and canceled it.
    Done {
        /// The sender of this packet part.
        sender_addr: SocketAddr,
        /// Whether the receiver was aware of this packets existence.
        ///
        /// This can be `false` if the receiver never received any of the part packets before the
        /// sender canceled the packet altogether.
        known_packet: bool,
    },
    /// A part of a sent packet was acked by its receiver.
    Ack {
        /// The receiver who acked this packet part.
        receiver_addr: SocketAddr,
        /// Whether the receiver has already acked this packet before, which can happen, if e.g. the
        /// sender had resent this part even though the client received it the first time and just
        /// acked it very late.
        duplicate: bool,
    },
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum PartInfo {
    /// A new part was received, but the packet is not yet fully reassembled.
    Pending,
    /// This was the last packet that was required to fully reassemble the packet.
    Reassembled,
    /// This part was already received before, which can happen if the corresponding `ack` did
    /// not reach the sender.
    ///
    /// Can still happen even after the packet has been reassembled.
    Duplicate,
}

#[derive(Debug, Error)]
pub enum HandlerError {
    #[error("recv failed: {0}")]
    Recv(io::Error),
    #[error("send to {receiver_addr} failed: {error}")]
    Send {
        receiver_addr: SocketAddr,
        error: io::Error,
    },
    #[error("packet from {sender_addr} failed validation: {error}")]
    PacketValidation {
        sender_addr: SocketAddr,
        error: Box<dyn Error>,
    },
    #[error("packet from {sender_addr} could not be handled: {error:?}")]
    Handle {
        sender_addr: SocketAddr,
        error: HandlePacketError,
    },
    #[error("{peer_addr} timed out")]
    Timeout { peer_addr: SocketAddr },
}

/// A connection handler that simply ignores all packets.
///
/// For servers, returns a default status and accepts all clients.
#[derive(Clone, Copy, Debug, Default, Hash, PartialEq, Eq)]
pub struct DefaultConnectionHandler;

impl ConnectionHandler for DefaultConnectionHandler {}

/// A connection handler that logs all events to `stdout`.
#[derive(Clone, Copy, Debug, Default, Hash, PartialEq, Eq)]
pub struct BasicLogConnectionHandler;

impl ConnectionHandler for BasicLogConnectionHandler {
    fn raw_packet(&mut self, packet: RawPacket) {
        println!("{packet:?}");
    }

    fn error(&mut self, error: HandlerError) {
        println!("{error:?}");
    }
}

#[derive(Clone, Copy, Debug)]
struct NonBlockingUdpSocket<'a> {
    socket: &'a UdpSocket,
    addr: SocketAddr,
}

impl<'a> NonBlockingUdpSocket<'a> {
    fn new(socket: &'a UdpSocket, addr: SocketAddr) -> Self {
        Self { socket, addr }
    }
}

impl NonBlocking for NonBlockingUdpSocket<'_> {
    fn send(self, buf: &[u8]) -> io::Result<bool> {
        match self.socket.send_to(buf, self.addr) {
            Ok(size) => {
                assert!(size == buf.len());
                Ok(true)
            }
            Err(error) => {
                if error.kind() == ErrorKind::WouldBlock {
                    Ok(false)
                } else {
                    Err(error)
                }
            }
        }
    }
}

pub(crate) fn recv<'a>(
    socket: &UdpSocket,
    packet_buf: &'a mut [u8; 512],
    handler: &mut impl ConnectionHandler,
) -> Option<Result<(SocketAddr, &'a [u8]), ()>> {
    Some(match socket.recv_from(packet_buf) {
        Ok((size, addr)) => Ok((addr, &packet_buf[..size])),
        Err(error) => {
            if error.kind() == ErrorKind::WouldBlock {
                return None;
            }
            handler.error(HandlerError::Recv(error));
            return Some(Err(()));
        }
    })
}
