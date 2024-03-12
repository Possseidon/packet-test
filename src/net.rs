pub mod client;
pub mod server;

use std::{error::Error, io, net::SocketAddr};

use thiserror::Error;

use crate::packet::HandlePacketError;

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
