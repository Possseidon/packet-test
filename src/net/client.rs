use std::{
    io,
    mem::replace,
    net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs, UdpSocket},
    time::Instant,
};

use rkyv::{validation::validators::DefaultValidator, Archive, CheckBytes};
use thiserror::Error;

use crate::packet::{
    ClientConnectionPacket, ClientPacketBuffers, ConnectionConfig, DefaultPackets, PacketId,
    Packets, ServerConnectionPacket,
};

use super::{ConnectionHandler, DefaultConnectionHandler, HandlerError};

// TODO: Make sure I've used swap_remove to remove entries from the servers vector

pub struct Client<P: Packets = DefaultPackets> {
    config: ConnectionConfig,
    socket: UdpSocket,
    state: ClientState,
    /// Contains servers that the client is listening to.
    ///
    /// Unless `state` is set to [`ClientState::Disconnected`], the first entry contains the
    /// connection to the server that the client is currently connected to. All other entries are
    /// just used for status queries.
    ///
    /// Should never contain duplicate server addresses.
    listeners: Vec<ServerListener<P>>,
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
            listeners: Default::default(),
        })
    }

    pub fn state(&self) -> ClientState {
        self.state
    }

    /// The server to which the client is connected to (or trying to connect to/disconnect from).
    pub fn server_addr(&self) -> Option<SocketAddr> {
        match self.state {
            ClientState::Disconnected => None,
            ClientState::Connecting | ClientState::Connected | ClientState::Disconnecting => Some(
                self.listeners
                    .first()
                    .expect("servers should not be empty")
                    .server_addr,
            ),
        }
    }

    /// Returns the compatibility to the given server.
    pub fn compatibility(&self, server_addr: SocketAddr) -> Option<Compatibility<P>> {
        self.listeners
            .iter()
            .find(|server| server.server_addr == server_addr)
            .map(|server| server.compatibility())
    }

    /// Starts sending version requests to the server if its compatibility is not known yet.
    pub fn listen(&mut self, server_addr: SocketAddr) -> Compatibility<P> {
        let (listener, _socket) = self.get_or_add_listener(server_addr);
        listener.compatibility()
    }

    /// Stops listening to the given server.
    ///
    /// Only works if the client is disconnected from the server.
    pub fn forget(&mut self, server_addr: SocketAddr) -> Result<(), ForgetError> {
        let index = self
            .listeners
            .iter()
            .position(|server| server.server_addr == server_addr);
        if let Some(index) = index {
            if index == 0 && self.state != ClientState::Disconnected {
                Err(ForgetError::NotDisconnected)
            } else {
                self.listeners.swap_remove(index);
                Ok(())
            }
        } else {
            Err(ForgetError::UnknownServer)
        }
    }

    /// Stops listening to all disconnected servers.
    pub fn forget_all(&mut self) {
        if self.state == ClientState::Disconnected {
            self.listeners.clear();
        } else {
            self.listeners.drain(1..);
        }
    }

    /// Sends a query request to the given server.
    pub fn query(
        &mut self,
        server_addr: SocketAddr,
        query: P::Query,
    ) -> Result<PacketId, QueryError> {
        let background_serialization_threshold = self.config.background_serialization_threshold;
        let (listener, socket) = self.get_or_add_listener(server_addr);
        match &mut listener.state {
            ServerState::PendingVersion { last_resend } => {
                Err(CompatibilityError::CompatibilityPending)?
            }
            ServerState::Compatible {
                buffers,
                last_interaction,
            } => Ok(buffers.send(
                ClientConnectionPacket::Query(query),
                background_serialization_threshold,
                |buf| socket.send_to(buf, server_addr),
            )?),
            ServerState::Incompatible(_) => Err(CompatibilityError::Incompatible)?,
        }
    }

    pub fn connect(
        &mut self,
        server_addr: impl ToSocketAddrs,
        connect: P::Connect,
    ) -> Result<PacketId, ConnectError> {
        if let ClientState::Disconnected = self.state {
            self.socket.connect(server_addr)?;
            let server_addr = self.socket.peer_addr()?;
            match &mut self.listeners[0].state {
                ServerState::PendingVersion { .. } => {
                    Err(CompatibilityError::CompatibilityPending)?
                }
                ServerState::Compatible { buffers, .. } => {
                    let packet_id = buffers.send(
                        ClientConnectionPacket::Connect(connect),
                        self.config.background_serialization_threshold,
                        |buf| self.socket.send_to(buf, server_addr),
                    )?;
                    self.state = ClientState::Connecting;
                    Ok(packet_id)
                }
                ServerState::Incompatible(_) => Err(CompatibilityError::Incompatible)?,
            }
        } else {
            Err(ConnectError::NotDisconnected)
        }
    }

    pub fn disconnect(&mut self, disconnect: P::Disconnect) -> Result<PacketId, SendError> {
        if let ClientState::Connected = self.state {
            let listener = &mut self.listeners[0];
            match &mut listener.state {
                ServerState::Compatible {
                    buffers,
                    last_interaction,
                } => {
                    let packet_id = buffers.send(
                        ClientConnectionPacket::Disconnect(disconnect),
                        self.config.background_serialization_threshold,
                        |buf| self.socket.send_to(buf, listener.server_addr),
                    );
                    match packet_id {
                        Ok(packet_id) => {
                            self.state = ClientState::Disconnecting;
                            Ok(packet_id)
                        }
                        Err(error) => {
                            self.listeners.swap_remove(0);
                            self.state = ClientState::Disconnected;
                            Err(error)?
                        }
                    }
                }
                ServerState::PendingVersion { .. } | ServerState::Incompatible(_) => {
                    panic!("connected server should be compatible")
                }
            }
        } else {
            Err(SendError::NotConnected)
        }
    }

    pub fn send(&mut self, packet: P::Client) -> Result<PacketId, SendError> {
        if let ClientState::Connected = self.state {
            let listener = &mut self.listeners[0];
            match &mut listener.state {
                ServerState::Compatible {
                    buffers,
                    last_interaction,
                } => {
                    let packet_id = buffers.send(
                        ClientConnectionPacket::User(packet),
                        self.config.background_serialization_threshold,
                        |buf| self.socket.send_to(buf, listener.server_addr),
                    );
                    match packet_id {
                        Ok(packet_id) => Ok(packet_id),
                        Err(error) => {
                            self.listeners.swap_remove(0);
                            self.state = ClientState::Disconnected;
                            Err(error)?
                        }
                    }
                }
                ServerState::PendingVersion { .. } | ServerState::Incompatible(_) => {
                    panic!("connected server should be compatible")
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
        let mut disconnected = self.state == ClientState::Disconnected;
        self.listeners.retain_mut(|server| {
            server.update(
                &mut self.socket,
                handler,
                replace(&mut disconnected, true),
                &mut self.state,
                &self.config,
            )
        });
    }

    fn get_or_add_listener(
        &mut self,
        server_addr: SocketAddr,
    ) -> (&mut ServerListener<P>, &mut UdpSocket) {
        let index = self
            .listeners
            .iter_mut()
            .position(|server| server.server_addr == server_addr);
        if let Some(index) = index {
            (&mut self.listeners[index], &mut self.socket)
        } else {
            self.listeners.push(ServerListener::new(server_addr));
            (
                self.listeners
                    .last_mut()
                    .expect("servers should not be empty"),
                &mut self.socket,
            )
        }
    }
}

/// The compatibility to a server.
pub enum Compatibility<'a, P: Packets> {
    /// Compatibility to the server has not yet been determined.
    Pending,
    /// The server is compatible and can be communicated with.
    Compatible,
    /// The server was determined to be incompatible, so there is no way to communicate.
    Incompatible(&'a P::CompatibilityError),
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
    fn is_compatible(&mut self, _version: P::Version) -> Result<(), P::CompatibilityError> {
        Ok(())
    }

    fn status(&mut self, _server_addr: SocketAddr, _status: &<P::Status as Archive>::Archived) {}
    fn accept(&mut self, _accept: &<P::Accept as Archive>::Archived) {}
    fn reject(&mut self, _reject: &<P::Reject as Archive>::Archived) {}
    fn kick(&mut self, _kick: &<P::Kick as Archive>::Archived) {}
    fn packet(&mut self, _packet: &<P::Server as Archive>::Archived) {}

    fn unexpected(&mut self, _server_addr: SocketAddr, _unexpected: UnexpectedServerPacket<P>) {}
}

impl<P: Packets> ClientHandler<P> for DefaultConnectionHandler {}

pub enum UnexpectedServerPacket<'a, P: Packets> {
    MalformedVersion,
    SendVersion,
    Accept(&'a <P::Accept as Archive>::Archived),
    Reject(&'a <P::Reject as Archive>::Archived),
    Kick(&'a <P::Kick as Archive>::Archived),
    Packet(&'a <P::Server as Archive>::Archived),
}

#[derive(Debug, Error)]
pub enum ForgetError {
    #[error("client is not disconnected from this server")]
    NotDisconnected,
    #[error("client was not listening to this server")]
    UnknownServer,
}

#[derive(Debug, Error)]
pub enum CompatibilityError {
    #[error("compatibility to the server has not yet been determined")]
    CompatibilityPending,
    #[error("server is incompatible with the client")]
    Incompatible,
}

#[derive(Debug, Error)]
pub enum QueryError {
    #[error(transparent)]
    Compatibility(#[from] CompatibilityError),
    #[error(transparent)]
    Io(#[from] io::Error),
}

#[derive(Debug, Error)]
pub enum ConnectError {
    #[error(transparent)]
    Compatibility(#[from] CompatibilityError),
    #[error("client can only connect while fully disconnected")]
    NotDisconnected,
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

// struct ListenerSocket<'a, P: Packets> {
//     listener: &'a mut ServerListener<P>,
//     socket: &'a mut UdpSocket,
// }

struct ServerListener<P: Packets> {
    server_addr: SocketAddr,
    state: ServerState<P>,
}

impl<P: Packets> ServerListener<P> {
    fn new(server_addr: SocketAddr) -> Self {
        Self {
            server_addr,
            state: ServerState::PendingVersion {
                last_resend: Some(Instant::now()),
            },
        }
    }

    fn compatibility(&self) -> Compatibility<P> {
        match &self.state {
            ServerState::PendingVersion { .. } => Compatibility::Pending,
            ServerState::Compatible { .. } => Compatibility::Compatible,
            ServerState::Incompatible(error) => Compatibility::Incompatible(error),
        }
    }

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
        // update buffers if they are compatible
        // - timeout old packets
        // - send pending packets
        match &mut self.state {
            ServerState::PendingVersion { .. } => {}
            ServerState::Compatible { buffers, .. } => {
                let result = buffers.update(config.timeout, config.resend_delay, |buf| {
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
            }
            ServerState::Incompatible(_) => {}
        }

        todo!()

        // let mut buf = [0; PACKET_BUFFER_SIZE];
        // loop {
        //     let (buf, server_addr) = match socket.recv_from(&mut buf) {
        //         Ok((size, server_addr)) => (&buf[..size], server_addr),
        //         Err(error) if error.kind() == ErrorKind::WouldBlock => {
        //             break true;
        //         }
        //         Err(error) => {
        //             handler.error(HandlerError::Recv(error));
        //             if !disconnected {
        //                 *state = ClientState::Disconnected;
        //             }
        //             break false;
        //         }
        //     };

        //     self.last_interaction = Some(Instant::now());

        //     // TODO: Can handle return WouldBlock?
        //     let packet_handling = self
        //         .state
        //         .handle(buf, |buf| socket.send_to(buf, server_addr));

        //     let packet_handling = match packet_handling {
        //         Ok(packet_handling) => packet_handling,
        //         Err(error) => {
        //             handler.error(HandlerError::Handle {
        //                 peer_addr: server_addr,
        //                 error,
        //             });
        //             continue;
        //         }
        //     };

        //     match packet_handling {
        //         PacketHandling::SendVersion => {
        //             handler.unexpected(server_addr, UnexpectedServerPacket::SendVersion);
        //         }
        //         PacketHandling::Version(version) => {
        //             let version = P::Version::read(version);
        //             match version {
        //                 Ok(version) => {
        //                     if handler.is_compatible(version) {
        //                         *state = ClientState::Connecting;
        //                     }
        //                 }
        //                 Err(MalformedVersion) => {
        //                     handler
        //                         .unexpected(server_addr, UnexpectedServerPacket::MalformedVersion);
        //                     *state = ClientState::Disconnected;
        //                 }
        //             }
        //         }
        //         PacketHandling::Received(ack) => match ack {
        //             ReceivedPacket::Pending { duplicate } => {
        //                 handler.pending(server_addr, duplicate);
        //             }
        //             ReceivedPacket::Reassembled(bytes) => {
        //                 let packet = match check_archived_root::<ServerConnectionPacket<P>>(bytes) {
        //                     Ok(packet) => packet,
        //                     Err(error) => {
        //                         handler.error(HandlerError::PacketValidation {
        //                             peer_addr: server_addr,
        //                             error: Box::new(error),
        //                         });
        //                         continue;
        //                     }
        //                 };
        //                 match packet {
        //                     ArchivedServerConnectionPacket::Status(status) => {
        //                         // does not need to be connected
        //                         handler.status(server_addr, status);
        //                     }
        //                     ArchivedServerConnectionPacket::Accept(accept) => {
        //                         if !disconnected
        //                             && *state == ClientState::Connecting
        //                             && server_addr == self.server_addr
        //                         {
        //                             handler.accept(accept);
        //                             *state = ClientState::Connected;
        //                             continue;
        //                         }
        //                         handler.unexpected(
        //                             server_addr,
        //                             UnexpectedServerPacket::Accept(accept),
        //                         );
        //                     }
        //                     ArchivedServerConnectionPacket::Reject(reject) => {
        //                         if !disconnected
        //                             && *state == ClientState::Connecting
        //                             && server_addr == self.server_addr
        //                         {
        //                             handler.reject(reject);
        //                             *state = ClientState::Disconnected;
        //                             continue;
        //                         }
        //                         handler.unexpected(
        //                             server_addr,
        //                             UnexpectedServerPacket::Reject(reject),
        //                         );
        //                     }
        //                     ArchivedServerConnectionPacket::Kick(kick) => {
        //                         if !disconnected
        //                             && *state == ClientState::Connected
        //                             && server_addr == self.server_addr
        //                         {
        //                             handler.kick(kick);
        //                             *state = ClientState::Disconnected;
        //                             continue;
        //                         }
        //                         handler.unexpected(server_addr, UnexpectedServerPacket::Kick(kick));
        //                     }
        //                     ArchivedServerConnectionPacket::User(packet) => {
        //                         if !disconnected
        //                             && *state == ClientState::Connected
        //                             && server_addr == self.server_addr
        //                         {
        //                             handler.packet(packet);
        //                             continue;
        //                         }
        //                         handler.unexpected(
        //                             server_addr,
        //                             UnexpectedServerPacket::Packet(packet),
        //                         );
        //                     }
        //                 }
        //             }
        //         },
        //         PacketHandling::Done => {
        //             handler.done(server_addr);
        //         }
        //         PacketHandling::Ack { duplicate } => {
        //             handler.ack(server_addr, duplicate);
        //         }
        //     }
        // }
    }
}

enum ServerState<P: Packets> {
    PendingVersion {
        last_resend: Option<Instant>,
    },
    Compatible {
        buffers: ClientPacketBuffers<P>,
        last_interaction: Instant,
    },
    Incompatible(P::CompatibilityError),
}

// TODO: disconnect on drop if the client is connected; do I really want that?
//       ... which requires moving the client to a separate thread to let the disconnect packet go through

// impl Drop for Client {
//     fn drop(&mut self) {
//         // try to notify the server, but it's fine if it fails
//         self.socket.send_to(&Disconnect.serialize()).ok();
//     }
// }
