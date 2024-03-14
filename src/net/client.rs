use std::{
    io::{self, ErrorKind},
    mem::replace,
    net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs, UdpSocket},
    time::Instant,
};

use rkyv::{check_archived_root, validation::validators::DefaultValidator, Archive, CheckBytes};
use thiserror::Error;

use crate::{
    net::NonBlockingUdpSocket,
    packet::{
        receive::ReceivedPacket, ArchivedServerConnectionPacket, ClientConnectionPacket,
        ClientPacketBuffers, ConnectionConfig, DefaultPackets, NonBlocking, NonZeroBatchSize,
        PacketBuffers, PacketHandling, PacketId, PacketKind, Packets, ServerConnectionPacket,
        VersionPacket, PACKET_BUFFER_SIZE,
    },
};

use super::{
    BasicLogConnectionHandler, ConnectionHandler, DefaultConnectionHandler, HandlerError, PartInfo,
    RawPacket,
};

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
    pub fn listen(&mut self, server_addr: SocketAddr) -> io::Result<Compatibility<P>> {
        let (listener, _socket) = self.get_or_add_listener(server_addr)?;
        Ok(listener.compatibility())
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

    /// Resets the compatibility state for the given server.
    pub fn refresh_all(&mut self) -> io::Result<()> {
        for listener in &mut self.listeners {
            listener.refresh(&self.socket)?;
        }
        Ok(())
    }

    /// Sends a query request to the given server.
    pub fn query(
        &mut self,
        server_addr: SocketAddr,
        query: P::Query,
    ) -> Result<PacketId, QueryError> {
        let background_serialization_threshold = self.config.background_serialization_threshold;
        let (listener, socket) = self.get_or_add_listener(server_addr)?;
        match &mut listener.state {
            ServerState::PendingVersion { .. } => Err(CompatibilityError::CompatibilityPending)?,
            ServerState::Compatible { buffers, .. } => Ok(buffers.send(
                ClientConnectionPacket::Query(query),
                background_serialization_threshold,
                NonBlockingUdpSocket::new(socket, server_addr),
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
            let background_serialization_threshold = self.config.background_serialization_threshold;
            let (listener, socket) = self.get_or_add_listener(server_addr)?;
            match &mut listener.state {
                ServerState::PendingVersion { .. } => {
                    Err(CompatibilityError::CompatibilityPending)?
                }
                ServerState::Compatible { buffers, .. } => {
                    let packet_id = buffers.send(
                        ClientConnectionPacket::Connect(connect),
                        background_serialization_threshold,
                        NonBlockingUdpSocket::new(socket, server_addr),
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
                ServerState::Compatible { buffers, .. } => {
                    let packet_id = buffers.send(
                        ClientConnectionPacket::Disconnect(disconnect),
                        self.config.background_serialization_threshold,
                        NonBlockingUdpSocket::new(&self.socket, listener.server_addr),
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
                ServerState::Compatible { buffers, .. } => {
                    let packet_id = buffers.send(
                        ClientConnectionPacket::User(packet),
                        self.config.background_serialization_threshold,
                        NonBlockingUdpSocket::new(&self.socket, listener.server_addr),
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
        let mut connected = self.state != ClientState::Disconnected;
        self.listeners.retain_mut(|listener| {
            listener.update(
                &self.socket,
                handler,
                replace(&mut connected, false).then_some(&mut self.state),
                &self.config,
            )
        });

        let mut buf = [0; PACKET_BUFFER_SIZE];
        loop {
            if self.listeners.is_empty() {
                break;
            }

            let (buf, server_addr) = match self.socket.recv_from(&mut buf) {
                Ok((size, server_addr)) => (&buf[..size], server_addr),
                Err(error) => {
                    if error.kind() == ErrorKind::WouldBlock {
                        break;
                    }
                    self.state = ClientState::Disconnected;
                    self.forget_all();
                    handler.error(HandlerError::Recv(error));
                    break;
                }
            };

            let Some(index) = self
                .listeners
                .iter_mut()
                .position(|listener| listener.server_addr == server_addr)
            else {
                handler.unexpected(server_addr, UnexpectedServerPacket::UnknownServer);
                continue;
            };

            // if there is a connection, the first entry is the connected server
            let is_connection_listener = index == 0 && self.state != ClientState::Disconnected;

            let listener = &mut self.listeners[index];
            match &mut listener.state {
                ServerState::PendingVersion { .. } => {
                    let Some(&kind) = buf.first() else {
                        handler.unexpected(server_addr, UnexpectedServerPacket::Empty);
                        continue;
                    };

                    let Ok(PacketKind::Version) = PacketKind::try_from(kind) else {
                        handler
                            .unexpected(server_addr, UnexpectedServerPacket::PendingCompatibility);
                        continue;
                    };

                    let Ok(version) = <P::Version as VersionPacket>::read(&buf[1..]) else {
                        handler.unexpected(server_addr, UnexpectedServerPacket::MalformedVersion);
                        continue;
                    };

                    match handler.compatibility(version) {
                        Ok(()) => listener
                            .state
                            .compatible(self.config.initial_send_batch_size),
                        Err(error) => listener.state.incompatible(error),
                    }
                }
                ServerState::Compatible {
                    buffers,
                    last_interaction,
                } => {
                    *last_interaction = Instant::now();

                    let packet_handling =
                        buffers.handle(buf, NonBlockingUdpSocket::new(&self.socket, server_addr));

                    let packet_handling = match packet_handling {
                        Ok(packet_handling) => packet_handling,
                        Err(error) => {
                            handler.error(HandlerError::Handle {
                                sender_addr: server_addr,
                                error,
                            });
                            continue;
                        }
                    };

                    match packet_handling {
                        PacketHandling::Received(ack) => match ack {
                            ReceivedPacket::Pending => {
                                handler.raw_packet(RawPacket::Part {
                                    sender_addr: server_addr,
                                    info: PartInfo::Pending,
                                });
                            }
                            ReceivedPacket::Duplicate => {
                                handler.raw_packet(RawPacket::Part {
                                    sender_addr: server_addr,
                                    info: PartInfo::Duplicate,
                                });
                            }
                            ReceivedPacket::Reassembled(bytes) => {
                                handler.raw_packet(RawPacket::Part {
                                    sender_addr: server_addr,
                                    info: PartInfo::Reassembled,
                                });

                                let packet =
                                    check_archived_root::<ServerConnectionPacket<P>>(bytes);
                                let packet = match packet {
                                    Ok(packet) => packet,
                                    Err(error) => {
                                        handler.error(HandlerError::PacketValidation {
                                            sender_addr: server_addr,
                                            error: Box::new(error),
                                        });
                                        continue;
                                    }
                                };
                                match packet {
                                    ArchivedServerConnectionPacket::Status(status) => {
                                        handler.status(server_addr, status);
                                    }
                                    ArchivedServerConnectionPacket::Accept(accept) => {
                                        if is_connection_listener
                                            && self.state == ClientState::Connecting
                                        {
                                            handler.accept(accept);
                                            self.state = ClientState::Connected;
                                            continue;
                                        }
                                        handler.unexpected(
                                            server_addr,
                                            UnexpectedServerPacket::NotConnected,
                                        );
                                    }
                                    ArchivedServerConnectionPacket::Reject(reject) => {
                                        if is_connection_listener
                                            && self.state == ClientState::Connecting
                                        {
                                            handler.reject(reject);
                                            self.state = ClientState::Disconnected;
                                            continue;
                                        }
                                        handler.unexpected(
                                            server_addr,
                                            UnexpectedServerPacket::NotConnected,
                                        );
                                    }
                                    ArchivedServerConnectionPacket::Kick(kick) => {
                                        if is_connection_listener
                                            && self.state == ClientState::Connected
                                        {
                                            handler.kick(kick);
                                            self.state = ClientState::Disconnected;
                                            continue;
                                        }
                                        handler.unexpected(
                                            server_addr,
                                            UnexpectedServerPacket::NotConnected,
                                        );
                                    }
                                    ArchivedServerConnectionPacket::User(packet) => {
                                        if is_connection_listener
                                            && self.state == ClientState::Connected
                                        {
                                            handler.packet(packet);
                                            continue;
                                        }
                                        handler.unexpected(
                                            server_addr,
                                            UnexpectedServerPacket::NotConnected,
                                        );
                                    }
                                }
                            }
                        },
                        PacketHandling::Done { known_packet } => {
                            handler.raw_packet(RawPacket::Done {
                                sender_addr: server_addr,
                                known_packet,
                            });
                        }
                        PacketHandling::Ack { duplicate } => {
                            handler.raw_packet(RawPacket::Ack {
                                receiver_addr: server_addr,
                                duplicate,
                            });
                        }
                    }
                }
                ServerState::Incompatible(_) => {
                    handler.unexpected(server_addr, UnexpectedServerPacket::Incompatible);
                }
            }
        }

        self.timeout_connection(handler);
    }

    fn timeout_connection(&mut self, handler: &mut impl ClientHandler<P>) {
        for listener in &mut self.listeners {
            match &mut listener.state {
                ServerState::PendingVersion { .. } => {}
                ServerState::Compatible {
                    last_interaction, ..
                } => {
                    if last_interaction.elapsed() > self.config.timeout {
                        self.state = ClientState::Disconnected;
                        handler.error(HandlerError::Timeout {
                            peer_addr: listener.server_addr,
                        });
                        if let Err(error) = listener.refresh(&self.socket) {
                            handler.error(HandlerError::Send {
                                receiver_addr: listener.server_addr,
                                error,
                            })
                        }
                    }
                }
                ServerState::Incompatible(_) => {}
            }
        }
    }

    fn get_or_add_listener(
        &mut self,
        server_addr: SocketAddr,
    ) -> io::Result<(&mut ServerListener<P>, &UdpSocket)> {
        let index = self
            .listeners
            .iter_mut()
            .position(|server| server.server_addr == server_addr);
        if let Some(index) = index {
            Ok((&mut self.listeners[index], &self.socket))
        } else {
            self.listeners
                .push(ServerListener::new(server_addr, &self.socket)?);
            Ok((
                self.listeners
                    .last_mut()
                    .expect("servers should not be empty"),
                &self.socket,
            ))
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

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
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
    /// Checks if the client is compatible with the version sent by the server.
    fn compatibility(&mut self, version: P::Version) -> Result<(), P::CompatibilityError> {
        _ = version;
        Ok(())
    }

    /// A server sent a status response.
    ///
    /// Unlike the other packets, this one is not limited to the connected server.
    fn status(&mut self, server_addr: SocketAddr, status: &<P::Status as Archive>::Archived) {
        _ = (server_addr, status);
    }
    /// The server has accepted the client.
    fn accept(&mut self, accept: &<P::Accept as Archive>::Archived) {
        _ = accept;
    }
    /// The server has rejected the client.
    fn reject(&mut self, reject: &<P::Reject as Archive>::Archived) {
        _ = reject;
    }
    /// The server has kicked the client.
    fn kick(&mut self, kick: &<P::Kick as Archive>::Archived) {
        _ = kick;
    }
    /// The server sent a user-defined packet.
    fn packet(&mut self, packet: &<P::Server as Archive>::Archived) {
        _ = packet;
    }

    /// A server sent an unexpected packet.
    fn unexpected(&mut self, server_addr: SocketAddr, unexpected: UnexpectedServerPacket) {
        _ = (server_addr, unexpected);
    }
}

impl<P: Packets> ClientHandler<P> for DefaultConnectionHandler {}

impl<P: Packets> ClientHandler<P> for BasicLogConnectionHandler
where
    P::Version: std::fmt::Debug,
    <P::Status as Archive>::Archived: std::fmt::Debug,
    <P::Accept as Archive>::Archived: std::fmt::Debug,
    <P::Reject as Archive>::Archived: std::fmt::Debug,
    <P::Kick as Archive>::Archived: std::fmt::Debug,
    <P::Server as Archive>::Archived: std::fmt::Debug,
{
    fn compatibility(&mut self, version: P::Version) -> Result<(), P::CompatibilityError> {
        println!("{version:?}");
        Ok(())
    }

    fn status(&mut self, server_addr: SocketAddr, status: &<P::Status as Archive>::Archived) {
        println!("{server_addr} {status:?}");
    }

    fn accept(&mut self, accept: &<P::Accept as Archive>::Archived) {
        println!("{accept:?}");
    }

    fn reject(&mut self, reject: &<P::Reject as Archive>::Archived) {
        println!("{reject:?}");
    }

    fn kick(&mut self, kick: &<P::Kick as Archive>::Archived) {
        println!("{kick:?}");
    }

    fn packet(&mut self, packet: &<P::Server as Archive>::Archived) {
        println!("{packet:?}");
    }

    fn unexpected(&mut self, server_addr: SocketAddr, unexpected: UnexpectedServerPacket) {
        println!("{server_addr} {unexpected:?}");
    }
}

#[derive(Debug, Error)]
pub enum UnexpectedServerPacket {
    #[error("not listening to this server")]
    UnknownServer,
    #[error("empty packet")]
    Empty,
    #[error("packet during pending compatible check")]
    PendingCompatibility,
    #[error("malformed version packet")]
    MalformedVersion,
    #[error("not connected to this server")]
    NotConnected,
    #[error("incompatible with this server")]
    Incompatible,
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
    fn new(server_addr: SocketAddr, socket: &UdpSocket) -> io::Result<Self> {
        Ok(Self {
            server_addr,
            state: Self::pending_version_query(socket, server_addr)?,
        })
    }

    fn refresh(&mut self, socket: &UdpSocket) -> io::Result<()> {
        self.state = Self::pending_version_query(socket, self.server_addr)?;
        Ok(())
    }

    fn compatibility(&self) -> Compatibility<P> {
        match &self.state {
            ServerState::PendingVersion { .. } => Compatibility::Pending,
            ServerState::Compatible { .. } => Compatibility::Compatible,
            ServerState::Incompatible(error) => Compatibility::Incompatible(error),
        }
    }

    /// Updates buffers if they are compatible and returns true if the listener should be kept.
    ///
    /// - Times out old packets
    /// - (Re)Send pending packets including version queries
    fn update(
        &mut self,
        socket: &UdpSocket,
        handler: &mut impl ClientHandler<P>,
        state: Option<&mut ClientState>,
        config: &ConnectionConfig,
    ) -> bool
    where
        <ServerConnectionPacket<P> as Archive>::Archived: for<'a> CheckBytes<DefaultValidator<'a>>,
    {
        match &mut self.state {
            ServerState::PendingVersion { last_version_query } => {
                if last_version_query.is_some_and(|it| it.elapsed() > config.resend_delay) {
                    match send_version_query(NonBlockingUdpSocket::new(socket, self.server_addr)) {
                        Ok(sent) => {
                            if sent {
                                *last_version_query = Some(Instant::now());
                            }
                            true
                        }
                        Err(error) => {
                            handler.error(HandlerError::Send {
                                receiver_addr: self.server_addr,
                                error,
                            });
                            false
                        }
                    }
                } else {
                    true
                }
            }
            ServerState::Compatible { buffers, .. } => {
                let result = buffers.update(
                    config.timeout,
                    config.resend_delay,
                    NonBlockingUdpSocket::new(socket, self.server_addr),
                );
                if let Err(error) = result {
                    handler.error(HandlerError::Send {
                        receiver_addr: self.server_addr,
                        error,
                    });
                    if let Some(state) = state {
                        *state = ClientState::Disconnected;
                    }
                    false
                } else {
                    true
                }
            }
            ServerState::Incompatible(_) => true,
        }
    }

    fn pending_version_query(
        socket: &UdpSocket,
        server_addr: SocketAddr,
    ) -> io::Result<ServerState<P>> {
        Ok(ServerState::PendingVersion {
            last_version_query: send_version_query(NonBlockingUdpSocket::new(socket, server_addr))?
                .then(Instant::now),
        })
    }
}

enum ServerState<P: Packets> {
    PendingVersion {
        last_version_query: Option<Instant>,
    },
    Compatible {
        buffers: ClientPacketBuffers<P>,
        // TODO: last_interaction is only interesting for timeouts if actually connected, but it
        //       doesn't hurt to update it for other servers as well
        last_interaction: Instant,
    },
    Incompatible(P::CompatibilityError),
}

impl<P: Packets> ServerState<P> {
    fn compatible(&mut self, initial_send_batch_size: NonZeroBatchSize) {
        *self = ServerState::Compatible {
            buffers: PacketBuffers::new(initial_send_batch_size),
            last_interaction: Instant::now(),
        };
    }

    fn incompatible(&mut self, error: P::CompatibilityError) {
        *self = ServerState::Incompatible(error);
    }
}

fn send_version_query(socket: impl NonBlocking) -> io::Result<bool> {
    socket.send(&[])
}

// TODO: disconnect on drop if the client is connected; do I really want that?
//       ... which requires moving the client to a separate thread to let the disconnect packet go through

// impl Drop for Client {
//     fn drop(&mut self) {
//         // try to notify the server, but it's fine if it fails
//         self.socket.send_to(&Disconnect.serialize()).ok();
//     }
// }
