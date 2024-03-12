use std::{
    io::{self, ErrorKind},
    net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs, UdpSocket},
    time::Instant,
};

use rkyv::{check_archived_root, validation::validators::DefaultValidator, Archive, CheckBytes};
use thiserror::Error;

use crate::packet::{
    receive::ReceivedPacket, ArchivedServerConnectionPacket, ClientConnectionPacket,
    ClientPacketBuffers, ConnectionConfig, DefaultPackets, PacketBuffers, PacketHandling, PacketId,
    Packets, ServerConnectionPacket, PACKET_BUFFER_SIZE,
};

use super::{ConnectionHandler, DefaultConnectionHandler, HandlerError};

pub struct Client<P: Packets = DefaultPackets> {
    config: ConnectionConfig,
    socket: UdpSocket,
    state: ClientState,
    /// Contains connections to servers.
    ///
    /// Unless `state` is set to [`ClientState::Disconnected`], the first entry contains the
    /// connection to the server that the client is currently connected to. All other entries are
    /// just used for status queries.
    ///
    /// Should never contain duplicate server addresses.
    servers: Vec<ServerConnection<P>>,
}

impl<P: Packets> Client<P> {
    const DEFAULT_ADDR: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);

    pub fn new() -> io::Result<Self> {
        Self::with_config(Self::DEFAULT_ADDR, Default::default())
    }

    pub fn with_config(
        client_addr: impl ToSocketAddrs,
        config: ConnectionConfig,
    ) -> io::Result<Self> {
        let socket = UdpSocket::bind(client_addr)?;
        socket.set_nonblocking(true)?;
        Ok(Self {
            config,
            socket,
            state: ClientState::Disconnected,
            servers: Default::default(),
        })
    }

    pub fn state(&self) -> ClientState {
        self.state
    }

    pub fn server_addr(&self) -> Option<SocketAddr> {
        match self.state {
            ClientState::Disconnected => None,
            ClientState::Connecting | ClientState::Connected | ClientState::Disconnecting => Some(
                self.servers
                    .first()
                    .expect("servers should not be empty")
                    .server_addr,
            ),
        }
    }

    pub fn query(&mut self, server_addr: SocketAddr, query: P::Query) -> io::Result<PacketId> {
        let background_serialization_threshold = self.config.background_serialization_threshold;
        let (socket, server) = self.server(server_addr);
        server.buffers.send(
            ClientConnectionPacket::Query(query),
            background_serialization_threshold,
            |buf| socket.send_to(buf, server_addr),
        )
    }

    pub fn connect(
        &mut self,
        server_addr: impl ToSocketAddrs,
        connect: P::Connect,
    ) -> Result<PacketId, ConnectError> {
        if let ClientState::Disconnected = self.state {
            self.socket.connect(server_addr)?;
            let server_addr = self.socket.peer_addr()?;
            let background_serialization_threshold = self.config.background_serialization_threshold;
            let (socket, server) = self.server(server_addr);
            let packet_id = server.buffers.send(
                ClientConnectionPacket::Connect(connect),
                background_serialization_threshold,
                |buf| socket.send_to(buf, server_addr),
            )?;
            self.state = ClientState::Connecting;
            Ok(packet_id)
        } else {
            Err(ConnectError::NotDisconnected)
        }
    }

    pub fn disconnect(&mut self, disconnect: P::Disconnect) -> Result<PacketId, DisconnectError> {
        if let ClientState::Connected = self.state {
            let server = self
                .servers
                .first_mut()
                .expect("servers should not be empty");
            let packet_id = server.buffers.send(
                ClientConnectionPacket::Disconnect(disconnect),
                self.config.background_serialization_threshold,
                |buf| self.socket.send_to(buf, server.server_addr),
            );
            match packet_id {
                Ok(packet_id) => {
                    self.state = ClientState::Disconnecting;
                    Ok(packet_id)
                }
                Err(error) => {
                    self.servers.remove(0);
                    self.state = ClientState::Disconnected;
                    Err(error)?
                }
            }
        } else {
            Err(DisconnectError::NotConnected)
        }
    }

    pub fn send(&mut self, packet: P::Client) -> Result<PacketId, SendError> {
        if let ClientState::Connected = self.state {
            let server = self
                .servers
                .first_mut()
                .expect("servers should not be empty");
            let packet_id = server.buffers.send(
                ClientConnectionPacket::User(packet),
                self.config.background_serialization_threshold,
                |buf| self.socket.send_to(buf, server.server_addr),
            );
            match packet_id {
                Ok(packet_id) => Ok(packet_id),
                Err(error) => {
                    self.servers.remove(0);
                    self.state = ClientState::Disconnected;
                    Err(error)?
                }
            }
        } else {
            Err(SendError::NotConnected)
        }
    }

    pub fn update(&mut self, handler: &mut impl ClientHandler<P>)
    where
        <ServerConnectionPacket<P> as Archive>::Archived: for<'a> CheckBytes<DefaultValidator<'a>>,
    {
        let disconnected = self.state == ClientState::Disconnected;
        self.servers.retain_mut(|server| {
            server.update(
                &mut self.socket,
                handler,
                disconnected,
                &mut self.state,
                &self.config,
            )
        });
    }

    /// Also returns the socket to appease the borrow checker.
    fn server(&mut self, server_addr: SocketAddr) -> (&mut UdpSocket, &mut ServerConnection<P>) {
        if let Some(index) = self
            .servers
            .iter_mut()
            .position(|server| server.server_addr == server_addr)
        {
            (&mut self.socket, &mut self.servers[index])
        } else {
            self.servers.push(ServerConnection {
                server_addr,
                buffers: PacketBuffers::new(self.config.initial_send_batch_size),
                awaiting_response_since: None,
            });
            (
                &mut self.socket,
                self.servers
                    .last_mut()
                    .expect("servers should not be empty"),
            )
        }
    }
}

#[derive(Debug, Error)]
pub enum ConnectError {
    #[error("client can only connect while fully disconnected")]
    NotDisconnected,
    #[error(transparent)]
    Io(#[from] io::Error),
}

#[derive(Debug, Error)]
pub enum DisconnectError {
    #[error("client can only disconnect while properly connected")]
    NotConnected,
    #[error(transparent)]
    Io(#[from] io::Error),
}

#[derive(Debug, Error)]
pub enum SendError {
    #[error("client can only send packets while connected")]
    NotConnected,
    #[error(transparent)]
    Io(#[from] io::Error),
}

struct ServerConnection<P: Packets> {
    server_addr: SocketAddr,
    buffers: ClientPacketBuffers<P>,
    awaiting_response_since: Option<Instant>,
}

impl<P: Packets> ServerConnection<P> {
    fn update(
        &mut self,
        socket: &mut UdpSocket,
        handler: &mut impl ClientHandler<P>,
        disconnected: bool,
        state: &mut ClientState,
        config: &ConnectionConfig,
    ) -> bool
    where
        <ServerConnectionPacket<P> as Archive>::Archived: for<'a> CheckBytes<DefaultValidator<'a>>,
    {
        let result = self
            .buffers
            .update(config.timeout, config.resend_delay, |buf| {
                socket.send_to(buf, self.server_addr)
            });
        if let Err(error) = result {
            handler.error(HandlerError::Send {
                peer_addr: self.server_addr,
                error,
            });
            if !disconnected {
                *state = ClientState::Disconnected;
            }
            return false;
        }

        let mut buf = [0; PACKET_BUFFER_SIZE];
        loop {
            let (buf, server_addr) = match socket.recv_from(&mut buf) {
                Ok((size, server_addr)) => (&buf[..size], server_addr),
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    break true;
                }
                Err(error) => {
                    handler.error(HandlerError::Recv(error));
                    if !disconnected {
                        *state = ClientState::Disconnected;
                    }
                    break false;
                }
            };

            self.awaiting_response_since = Some(Instant::now());

            // TODO: Can handle return WouldBlock?
            let packet_handling = self
                .buffers
                .handle(buf, |buf| socket.send_to(buf, server_addr));

            let packet_handling = match packet_handling {
                Ok(packet_handling) => packet_handling,
                Err(error) => {
                    handler.error(HandlerError::Handle {
                        peer_addr: server_addr,
                        error,
                    });
                    continue;
                }
            };

            match packet_handling {
                PacketHandling::Received(ack) => match ack {
                    ReceivedPacket::Pending { duplicate } => {
                        handler.pending(server_addr, duplicate);
                    }
                    ReceivedPacket::Reassembled(bytes) => {
                        let packet = match check_archived_root::<ServerConnectionPacket<P>>(bytes) {
                            Ok(packet) => packet,
                            Err(error) => {
                                handler.error(HandlerError::PacketValidation {
                                    peer_addr: server_addr,
                                    error: Box::new(error),
                                });
                                continue;
                            }
                        };
                        match packet {
                            ArchivedServerConnectionPacket::Status(status) => {
                                // does not need to be connected
                                handler.status(server_addr, status);
                            }
                            ArchivedServerConnectionPacket::Accept(accept) => {
                                if !disconnected
                                    && *state == ClientState::Connecting
                                    && server_addr == self.server_addr
                                {
                                    handler.accept(accept);
                                    *state = ClientState::Connected;
                                    continue;
                                }
                                handler.unexpected(
                                    server_addr,
                                    UnexpectedServerPacket::Accept(accept),
                                );
                            }
                            ArchivedServerConnectionPacket::Reject(reject) => {
                                if !disconnected
                                    && *state == ClientState::Connecting
                                    && server_addr == self.server_addr
                                {
                                    handler.reject(reject);
                                    *state = ClientState::Disconnected;
                                    continue;
                                }
                                handler.unexpected(
                                    server_addr,
                                    UnexpectedServerPacket::Reject(reject),
                                );
                            }
                            ArchivedServerConnectionPacket::Kick(kick) => {
                                if !disconnected
                                    && *state == ClientState::Connected
                                    && server_addr == self.server_addr
                                {
                                    handler.kick(kick);
                                    *state = ClientState::Disconnected;
                                    continue;
                                }
                                handler.unexpected(server_addr, UnexpectedServerPacket::Kick(kick));
                            }
                            ArchivedServerConnectionPacket::User(packet) => {
                                if !disconnected
                                    && *state == ClientState::Connected
                                    && server_addr == self.server_addr
                                {
                                    handler.packet(packet);
                                    continue;
                                }
                                handler.unexpected(
                                    server_addr,
                                    UnexpectedServerPacket::Packet(packet),
                                );
                            }
                        }
                    }
                },
                PacketHandling::Done => {
                    handler.done(server_addr);
                }
                PacketHandling::Ack { duplicate } => {
                    handler.ack(server_addr, duplicate);
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClientState {
    /// Client is not connected to a server.
    Disconnected,
    /// Client is awaiting acceptance by the server.
    Connecting,
    /// Client was accepted by the server and is connected.
    Connected,
    /// Client is waiting for the server to acknowledge it has disconnected.
    Disconnecting,
}

pub trait ClientHandler<P: Packets>: ConnectionHandler {
    fn status(&mut self, _server_addr: SocketAddr, _status: &<P::Status as Archive>::Archived) {}
    fn accept(&mut self, _accept: &<P::Accept as Archive>::Archived) {}
    fn reject(&mut self, _reject: &<P::Reject as Archive>::Archived) {}
    fn kick(&mut self, _kick: &<P::Kick as Archive>::Archived) {}
    fn packet(&mut self, _packet: &<P::Server as Archive>::Archived) {}

    fn unexpected(&mut self, _server_addr: SocketAddr, _unexpected: UnexpectedServerPacket<P>) {}
}

impl<P: Packets> ClientHandler<P> for DefaultConnectionHandler {}

pub enum UnexpectedServerPacket<'a, P: Packets> {
    Accept(&'a <P::Accept as Archive>::Archived),
    Reject(&'a <P::Reject as Archive>::Archived),
    Kick(&'a <P::Kick as Archive>::Archived),
    Packet(&'a <P::Server as Archive>::Archived),
}

// TODO: disconnect on drop if the client is connected; do I really want that?
//       ... which requires moving the client to a separate thread to let the disconnect packet go through

// impl Drop for Client {
//     fn drop(&mut self) {
//         // try to notify the server, but it's fine if it fails
//         self.socket.send_to(&Disconnect.serialize()).ok();
//     }
// }
