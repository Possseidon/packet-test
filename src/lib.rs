pub(crate) mod packet;

use std::{
    io::{self, ErrorKind},
    iter::once,
    net::{Ipv4Addr, SocketAddr, UdpSocket},
    str::Utf8Error,
    time::{Duration, Instant},
};

use bevy::prelude::*;
use num_enum::{IntoPrimitive, TryFromPrimitive};
use packet::{NonZeroBatchSize, ReceivePacketBuffer, SendPacketBuffer, PACKET_BUFFER_SIZE};
use thiserror::Error;
use uuid::Uuid;

#[derive(Component)]
pub struct ServerEntity(pub Uuid);

pub const DEFAULT_PORT: u16 = 42069;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Connection {
    addr: SocketAddr,
    ping_uuid: Uuid,
    ping: Instant,
    pong: Option<Instant>,
    batch_size: NonZeroBatchSize,
    send_buffer: SendPacketBuffer,
    receive_buffer: ReceivePacketBuffer,
    // TODO: Add a way to track which entities and things are known to be up to date:
    // updates: UpdateTracker,
}

#[derive(Resource)]
pub struct Server {
    socket: UdpSocket,
    /// How long a client has time to respond to a ping before getting disconnected.
    timeout: Duration,
    /// How long to wait until sending the next ping after a client has ponged.
    ping_delay: Duration,
    /// All connections to clients.
    ///
    /// A client cannot move position, since the order is also used by entity change tracking.
    connections: Vec<Option<Connection>>,
}

impl Server {
    pub fn new() -> io::Result<Self> {
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, DEFAULT_PORT))?;
        socket.set_nonblocking(true)?;
        Ok(Self {
            socket,
            timeout: Duration::from_secs(10),
            ping_delay: Duration::from_secs(3),
            connections: default(),
        })
    }

    fn connect(&mut self, addr: SocketAddr) -> Result<(), AlreadyConnected> {
        if self.connection_mut(addr).is_ok() {
            return Err(AlreadyConnected(addr));
        }

        let send = |buf| self.socket.send_to(buf, addr);

        self.socket.send_to(&Accept.serialize(), addr);

        let ping_uuid = Uuid::new_v4();
        let connection = Some(Connection {
            addr,
            ping_uuid,
            ping: Instant::now(),
            pong: None,
            batch_size: NonZeroBatchSize::new(16).unwrap(),
            send_buffer: default(),
            receive_buffer: default(),
        });

        self.socket.send_to(&Ping(ping_uuid).serialize(), addr);

        if let Some(pos) = self.connections.iter().position(Option::is_none) {
            self.connections[pos] = connection;
        } else {
            self.connections.push(connection);
        };
        Ok(())
    }

    fn connection_mut(&mut self, addr: SocketAddr) -> Result<&mut Connection, NotConnected> {
        self.connections
            .iter_mut()
            .flatten()
            .find(|connection| connection.addr == addr)
            .ok_or(NotConnected(addr))
    }

    fn disconnect(&mut self, addr: SocketAddr) -> Result<(), NotConnected> {
        let connection = self
            .connections
            .iter_mut()
            .find(|connection| connection.is_some_and(|connection| connection.addr == addr));
        if let Some(connection) = connection {
            // TODO: Go through all entities and clear this client, so that it is clean for a new connection
            *connection = None;
            Ok(())
        } else {
            Err(NotConnected(addr))
        }
    }

    fn handle_client_packet(&mut self, packet: ClientPacket, addr: SocketAddr) {
        match packet {
            ClientPacket::Pong(uuid) => {
                if let Ok(connection) = self.connection_mut(addr) {
                    if connection.ping_uuid == uuid {
                        connection.pong = Some(Instant::now());
                        info!("{addr} ponged after {:?}", connection.ping.elapsed());
                    } else {
                        warn!(
                            "{addr}: ignored pong with incorrect UUID ({} expected, got {uuid})",
                            connection.ping_uuid
                        );
                    }
                } else {
                    warn!("{addr}: ignored pong from unknown client");
                }
            }
            ClientPacket::Connect => match self.connect(addr) {
                Ok(()) => {
                    info!("{addr} connected");
                }
                Err(AlreadyConnected(_)) => {
                    info!("{addr}: ignored connect from already connected client");
                }
            },
            ClientPacket::Disconnect => match self.disconnect(addr) {
                Ok(()) => {
                    info!("{addr} disconnected");
                }
                Err(NotConnected(_)) => {
                    info!("{addr}: ignored disconnect from unknown client");
                }
            },
        }
    }

    /// Disconnects any clients that haven't ponged in a while.
    fn timeout_clients(&mut self) {
        for connection_entry in &mut self.connections {
            if let Some(connection) = connection_entry {
                if connection.pong.is_none() && connection.ping.elapsed() > self.timeout {
                    warn!("{:?} timed out", connection.addr);
                    // notify the client, in case it is still listening and just can't respond
                    self.socket.send_to(&Kick.serialize(), connection.addr).ok();
                    *connection_entry = None;
                }
            }
        }
    }

    /// Pings clients that have not been pinged in a while.
    fn ping_clients(&mut self) {
        for connection in self.connections.iter_mut().flatten() {
            if let Some(pong) = connection.pong {
                if pong.elapsed() > self.ping_delay {
                    connection.ping = Instant::now();
                    connection.ping_uuid = Uuid::new_v4();
                    connection.pong = None;

                    self.socket
                        .send_to(&Ping(connection.ping_uuid).serialize(), connection.addr);
                }
            }
        }
    }

    /// Sends a reliable packet to the given client and returns an id that can be used to track it.
    fn send(&self, addr: SocketAddr, packet: impl ServerPacketPayload) -> Result<Uuid, ()> {
        if let Ok(connection) = self.connection_mut(addr) {
            Ok(connection.send_buffer.send(
                packet.payload(),
                |buf| self.socket.send_to(buf, addr),
                connection.batch_size,
            ))
        } else {
            todo!("client no longer connected");
            Err(())
        }
    }

    /// Sends an unreliable update packet to the given client.
    fn update(&self, addr: SocketAddr, packet: impl ServerUpdatePayload) -> io::Result<()> {
        self.update_many(addr, once(packet))?;
        Ok(())
    }

    /// Sends a bunch of unreliable update packets to the given client.
    ///
    /// This is more efficient than sending one at a time, since it can reuse the same buffer.
    fn update_many<T: ServerUpdatePayload>(
        &self,
        addr: SocketAddr,
        packets: impl Iterator<Item = T>,
    ) -> io::Result<()> {
        let mut buf = [0; PACKET_BUFFER_SIZE];
        for packet in packets {
            self.socket.send_to(packet.payload(&mut buf), addr)?;
        }
        Ok(())
    }
}

trait PacketPayload {
    fn payload(self) -> Vec<u8>;
}

trait ServerUpdatePayload {
    fn payload<'a>(&self, buf: &'a mut [u8]) -> &'a [u8];
}

trait ServerPacketPayload: PacketPayload {}

trait ClientPacketPayload: PacketPayload {}

#[derive(Debug, Error)]
#[error("{0} is already connected")]
struct AlreadyConnected(SocketAddr);

#[derive(Debug, Error)]
#[error("{0} is not connected")]
struct NotConnected(SocketAddr);

pub fn handle_client_packets(mut server: ResMut<Server>) {
    let mut buf = [0; PACKET_BUFFER_SIZE];
    loop {
        let (payload, addr) = match server.socket.recv_from(&mut buf) {
            Ok((size, addr)) => (&buf[..size], addr),
            Err(error) => match error.kind() {
                ErrorKind::WouldBlock => break,
                ErrorKind::ConnectionReset => {
                    // we don't know which client disconnected, so just let it time out
                    continue;
                }
                _ => {
                    error!("{error}");
                    continue;
                }
            },
        };
        match ClientPacket::deserialize(payload) {
            Ok(packet) => {
                server.handle_client_packet(packet, addr);
            }
            Err(error) => {
                error!("{error}");
            }
        }
    }
}

pub fn timeout_clients(mut server: ResMut<Server>) {
    server.timeout_clients();
}

pub fn ping_clients(mut server: ResMut<Server>) {
    server.ping_clients();
}

#[derive(Debug, Error)]
pub enum PacketDeserializationError {
    #[error("empty packet")]
    Empty,
    #[error("malformed packet")]
    Malformed,
    #[error("invalid packet kind: {0}")]
    InvalidKind(u8),
    #[error(transparent)]
    Utf8Error(#[from] Utf8Error),
}

/// Packets sent by the client.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClientPacket {
    Pong(Uuid),
    Connect,
    Disconnect,
}

impl ClientPacket {
    fn deserialize(payload: &[u8]) -> Result<Self, PacketDeserializationError> {
        let Some(&kind) = payload.first() else {
            return Err(PacketDeserializationError::Empty);
        };

        let Ok(kind) = kind.try_into() else {
            return Err(PacketDeserializationError::InvalidKind(kind));
        };

        match kind {
            ClientPacketKind::Pong => Ok(Self::Pong(Uuid::from_bytes(
                payload
                    .get(1..17)
                    .ok_or(PacketDeserializationError::Malformed)?
                    .try_into()
                    .expect("slice should be 16 bytes long"),
            ))),
            ClientPacketKind::Connect => Ok(Self::Connect),
            ClientPacketKind::Disconnect => Ok(Self::Disconnect),
        }
    }
}

/// Reliable packets that are sent by the server.
#[derive(Clone, Copy, Debug, PartialEq, Eq, IntoPrimitive, TryFromPrimitive)]
#[repr(u8)]
enum ServerPacketKind {
    Ping,
    Accept,
    Reject,
    Kick,
}

/// Unreliable packets that are sent by the server.
enum UnreliableServerPacketKind {}

/// Reliable packets that are sent by the client.
#[derive(Clone, Copy, Debug, PartialEq, Eq, IntoPrimitive, TryFromPrimitive)]
#[repr(u8)]
enum ClientPacketKind {
    Pong,
    Connect,
    Disconnect,
}

/// Packets sent by the server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServerPacket {
    Ping(Uuid),
    Accept,
    Reject,
    Kick,
}

pub struct Ping(pub Uuid);

pub struct Pong(pub Uuid);

/// The client wants to connect to the server.
///
/// TODO: This needs some token so that the server can identify the client.
pub struct Connect;

/// The client wants to disconnect from the server.
pub struct Disconnect;

/// The server accepts a client that wants to connect.
pub struct Accept;

/// The server rejects a client that wants to connect.
pub struct Reject;

/// The server kicks an already accepted client; also sent to all clients when the server shuts down.
pub struct Kick;
