pub mod client;
pub mod server;

use std::{
    error::Error,
    io::{self, ErrorKind},
    net::{SocketAddr, UdpSocket},
};

use thiserror::Error;

use crate::packet::{HandlePacketError, NonBlocking};

pub trait ConnectionHandler {
    fn pending(&mut self, _peer_addr: SocketAddr, _duplicate: bool) {}
    fn done(&mut self, _peer_addr: SocketAddr) {}
    fn ack(&mut self, _peer_addr: SocketAddr, _duplicate: bool) {}

    fn error(&mut self, _error: HandlerError) {}
}

#[derive(Debug, Error)]
pub enum HandlerError {
    #[error("recv failed: {0}")]
    Recv(io::Error),
    #[error("send to {peer_addr} failed: {error}")]
    Send {
        peer_addr: SocketAddr,
        error: io::Error,
    },
    #[error("received invalid packet from {peer_addr}: {error:?}")]
    PacketValidation {
        peer_addr: SocketAddr,
        error: Box<dyn Error>,
    },
    #[error("error handling packet from {peer_addr}: {error:?}")]
    Handle {
        peer_addr: SocketAddr,
        error: HandlePacketError,
    },
}

/// A connection handler that simply ignores all packets.
///
/// For servers, returns a default status and accepts all clients.
pub struct DefaultConnectionHandler;

impl ConnectionHandler for DefaultConnectionHandler {}

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
