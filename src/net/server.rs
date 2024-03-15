use std::{
    collections::BTreeMap,
    io,
    net::{SocketAddr, ToSocketAddrs, UdpSocket},
    time::Instant,
};

use rkyv::{check_archived_root, validation::validators::DefaultValidator, Archive, CheckBytes};
use thiserror::Error;
use uuid::Uuid;

use crate::{
    config::ServerConfig,
    packet::{
        receive::ReceivedPacket, ArchivedClientConnectionPacket, ClientConnectionPacket,
        DefaultPackets, HandlePacketError, NonBlocking, PacketHandling, PacketId, PacketKind,
        Packets, ServerConnectionPacket, ServerPacketBuffers, VersionPacket, PACKET_BUFFER_SIZE,
    },
};

use super::{
    recv, BasicLogConnectionHandler, ConnectionHandler, DefaultConnectionHandler, HandlerError,
    NonBlockingUdpSocket, PartInfo, RawPacket,
};

pub struct Server<P: Packets = DefaultPackets> {
    /// The configuration of the server.
    config: ServerConfig,
    /// The single UDP socket used for communiating with all clients.
    socket: UdpSocket,
    /// All clients that the server is listening to, excluding the ones that are connected.
    ///
    /// Clients get added to this list by sending a version query (an empty packet) and get removed
    /// after [`ServerConfig::listener_timeout`]. Clients in this list are allowed to send status
    /// queries and try connecting to the server.
    listeners: BTreeMap<SocketAddr, ServerPacketBuffers<P>>,
    /// All clients that are connected to the server.
    ///
    /// Listeners are upgraded to clients (and moved to this list) when they try to connect.
    /// Connected clients cannot move position, since the order is also used by entity change
    /// tracking. Instead, they are simply set to [`None`] upon disconnection and can be reused by
    /// new clients.
    clients: Vec<Option<ClientConnection<P>>>,
}

impl<P: Packets> Server<P> {
    pub fn host(server_addr: impl ToSocketAddrs) -> io::Result<Self> {
        Self::host_with_config(server_addr, Default::default())
    }

    pub fn host_with_config(
        server_addr: impl ToSocketAddrs,
        config: ServerConfig,
    ) -> io::Result<Self> {
        let socket = UdpSocket::bind(server_addr)?;
        socket.set_nonblocking(true)?;
        Ok(Self {
            config,
            socket,
            listeners: Default::default(),
            clients: Default::default(),
        })
    }

    pub fn unlisten(&mut self, client_addr: SocketAddr) -> Result<bool, UnlistenError> {
        if self.listeners.remove(&client_addr).is_some() {
            Ok(true)
        } else if self
            .clients
            .iter()
            .flatten()
            .any(|client| client.client_addr == client_addr)
        {
            Err(UnlistenError)
        } else {
            Ok(false)
        }
    }

    pub fn unlisten_all(&mut self) {
        self.listeners.clear();
    }

    pub fn client(&mut self, client_addr: SocketAddr) -> Option<Client<P>> {
        self.clients
            .iter_mut()
            .flatten()
            .find(|client| client.client_addr == client_addr)
            .map(|client| Client {
                socket: &self.socket,
                client_addr,
                buffers: &mut client.buffers,
                background_serialization_threshold: self
                    .config
                    .connection
                    .background_serialization_threshold,
            })
    }

    pub fn update(&mut self, handler: &mut impl ServerHandler<P>)
    where
        <ClientConnectionPacket<P> as Archive>::Archived: for<'a> CheckBytes<DefaultValidator<'a>>,
    {
        self.update_buffers(handler);
        self.handle_packets(handler);
        self.send_pings(handler);
    }

    /// Updates client buffers so they send pending packets and time out stale ones.
    fn update_buffers(&mut self, handler: &mut impl ServerHandler<P>) {
        let mut first_empty_slot = 0;
        for (index, client_entry) in self.clients.iter_mut().enumerate() {
            let Some(client) = client_entry else { continue };
            first_empty_slot = index + 1;
            let result = client.buffers.update(
                self.config.connection.resend_delay,
                self.config.connection.max_missed_acks,
                self.config.connection.receive_timeout,
                NonBlockingUdpSocket::new(&self.socket, client.client_addr),
            );
            if let Err(error) = result {
                handler.error(HandlerError::Send {
                    receiver_addr: client.client_addr,
                    error,
                });
                *client_entry = None;
            }
        }
        self.clients.truncate(first_empty_slot);
        // TODO: self.clients.shrink_to_fit() if capacity is vastly bigger than len
    }

    /// Deals with incoming packets.
    fn handle_packets(&mut self, handler: &mut impl ServerHandler<P>)
    where
        <ClientConnectionPacket<P> as Archive>::Archived: for<'a> CheckBytes<DefaultValidator<'a>>,
    {
        let mut packet_buf = [0; PACKET_BUFFER_SIZE];
        while let Some((client_addr, buf)) = recv(&self.socket, &mut packet_buf, handler) {
            if buf.is_empty() {
                self.handle_version_request(
                    &mut packet_buf,
                    handler,
                    client_addr,
                    self.listeners.contains_key(&client_addr),
                );
                continue;
            }

            // look for clients first (most packets should generally come from connected clients)
            if let Some(client_entry) = self.clients.iter_mut().find(|client_entry| {
                client_entry
                    .as_ref()
                    .is_some_and(|client| client.client_addr == client_addr)
            }) {
                let client = client_entry.as_mut().unwrap();

                if PacketKind::try_from(buf[0]) == Ok(PacketKind::Ping) {
                    let Some(active_ping) = client.active_ping else {
                        handler.pong(client_addr, false);
                        continue;
                    };
                    let Ok(ping_id) = Uuid::from_slice(&buf[1..]) else {
                        handler.error(HandlerError::Handle {
                            sender_addr: client_addr,
                            error: HandlePacketError::Malformed,
                        });
                        continue;
                    };
                    if ping_id == active_ping.ping_id {
                        client.last_pong = Some(Instant::now());
                        client.missed_pongs = 0;
                        handler.pong(client_addr, true);
                    } else {
                        handler.pong(client_addr, false);
                    }
                    continue;
                }

                let buffers = &mut client.buffers;
                let Some(packet) =
                    Self::handle_buffers(buffers, buf, &self.socket, client_addr, handler)
                else {
                    continue;
                };

                let disconnect = match packet {
                    ArchivedClientConnectionPacket::Query(query) => {
                        let status = handler.query(client_addr, query);
                        let result = buffers.send(
                            ServerConnectionPacket::Status(status),
                            self.config.connection.background_serialization_threshold,
                            NonBlockingUdpSocket::new(&self.socket, client_addr),
                        );
                        if let Err(error) = result {
                            handler.error(HandlerError::Send {
                                receiver_addr: client_addr,
                                error,
                            });
                            true
                        } else {
                            false
                        }
                    }
                    ArchivedClientConnectionPacket::Connect(_) => {
                        handler.unexpected(client_addr, UnexpectedClientPacket::AlreadyConnected);
                        true
                    }
                    ArchivedClientConnectionPacket::Disconnect(disconnect) => {
                        handler.disconnect(client_addr, disconnect);
                        true
                    }
                    ArchivedClientConnectionPacket::User(user) => {
                        handler.packet(client_addr, user);
                        false
                    }
                };

                if disconnect {
                    let client = client_entry.take().unwrap();
                    self.listeners.insert(client.client_addr, client.buffers);
                }

                continue;
            }

            if let Some(buffers) = self.listeners.get_mut(&client_addr) {
                let Some(packet) =
                    Self::handle_buffers(buffers, buf, &self.socket, client_addr, handler)
                else {
                    continue;
                };

                match packet {
                    ArchivedClientConnectionPacket::Query(query) => {
                        let status = handler.query(client_addr, query);
                        let result = buffers.send(
                            ServerConnectionPacket::Status(status),
                            self.config.connection.background_serialization_threshold,
                            NonBlockingUdpSocket::new(&self.socket, client_addr),
                        );
                        if let Err(error) = result {
                            handler.error(HandlerError::Send {
                                receiver_addr: client_addr,
                                error,
                            });
                        }
                    }
                    ArchivedClientConnectionPacket::Connect(connect) => {
                        match handler.connect(client_addr, connect) {
                            Connect::Accept(accept) => {
                                let result = buffers.send(
                                    ServerConnectionPacket::Accept(accept),
                                    self.config.connection.background_serialization_threshold,
                                    NonBlockingUdpSocket::new(&self.socket, client_addr),
                                );
                                if let Err(error) = result {
                                    handler.error(HandlerError::Send {
                                        receiver_addr: client_addr,
                                        error,
                                    });
                                } else {
                                    *self.get_or_add_free_slot() = Some(ClientConnection::new(
                                        client_addr,
                                        self.listeners.remove(&client_addr).unwrap(),
                                    ));
                                }
                            }
                            Connect::Reject(reject) => {
                                let result = buffers.send(
                                    ServerConnectionPacket::Reject(reject),
                                    self.config.connection.background_serialization_threshold,
                                    NonBlockingUdpSocket::new(&self.socket, client_addr),
                                );
                                if let Err(error) = result {
                                    handler.error(HandlerError::Send {
                                        receiver_addr: client_addr,
                                        error,
                                    });
                                }
                            }
                        }
                    }
                    ArchivedClientConnectionPacket::Disconnect(_)
                    | ArchivedClientConnectionPacket::User(_) => {
                        handler.unexpected(client_addr, UnexpectedClientPacket::NotConnected)
                    }
                }

                continue;
            }

            handler.unexpected(client_addr, UnexpectedClientPacket::UnknownCompatibility);
        }
    }

    fn get_or_add_free_slot(&mut self) -> &mut Option<ClientConnection<P>> {
        if let Some(index) = self.clients.iter().position(Option::is_none) {
            &mut self.clients[index]
        } else {
            self.clients.push(None);
            self.clients.last_mut().unwrap()
        }
    }

    fn handle_version_request(
        &mut self,
        packet_buf: &mut [u8; PACKET_BUFFER_SIZE],
        handler: &mut impl ServerHandler<P>,
        client_addr: SocketAddr,
        listening: bool,
    ) {
        match Self::send_version(&self.socket, packet_buf, handler, client_addr) {
            Ok(sent) => {
                if sent
                    && !listening
                    && !self
                        .clients
                        .iter()
                        .flatten()
                        .any(|client| client.client_addr == client_addr)
                {
                    self.listeners.insert(
                        client_addr,
                        ServerPacketBuffers::new(self.config.connection.initial_send_batch_size),
                    );
                }
            }
            Err(error) => {
                handler.error(HandlerError::Send {
                    receiver_addr: client_addr,
                    error,
                });
            }
        }
    }

    fn send_version(
        socket: &UdpSocket,
        packet_buf: &mut [u8; PACKET_BUFFER_SIZE],
        handler: &mut impl ServerHandler<P>,
        client_addr: SocketAddr,
    ) -> io::Result<bool> {
        packet_buf[0] = PacketKind::Version.into();
        let version_len = handler.version().write(&mut packet_buf[1..]);
        let buf = &packet_buf[..version_len + 1];
        NonBlockingUdpSocket::new(socket, client_addr).send(buf)
    }

    fn handle_buffers<'a>(
        buffers: &'a mut ServerPacketBuffers<P>,
        buf: &'a [u8],
        socket: &UdpSocket,
        client_addr: SocketAddr,
        handler: &mut impl ServerHandler<P>,
    ) -> Option<&'a ArchivedClientConnectionPacket<P>>
    where
        <ClientConnectionPacket<P> as Archive>::Archived: for<'b> CheckBytes<DefaultValidator<'b>>,
    {
        let packet_handling = buffers.handle(buf, NonBlockingUdpSocket::new(socket, client_addr));
        let packet_handling = match packet_handling {
            Ok(packet_handling) => packet_handling,
            Err(error) => {
                handler.error(HandlerError::Handle {
                    sender_addr: client_addr,
                    error,
                });
                return None;
            }
        };

        match packet_handling {
            PacketHandling::Received(packet) => match packet {
                ReceivedPacket::Pending => handler.raw_packet(RawPacket::Part {
                    sender_addr: client_addr,
                    info: PartInfo::Pending,
                }),
                ReceivedPacket::Duplicate => handler.raw_packet(RawPacket::Part {
                    sender_addr: client_addr,
                    info: PartInfo::Duplicate,
                }),
                ReceivedPacket::Reassembled(bytes) => {
                    handler.raw_packet(RawPacket::Part {
                        sender_addr: client_addr,
                        info: PartInfo::Reassembled,
                    });

                    let packet = check_archived_root::<ClientConnectionPacket<P>>(bytes);
                    match packet {
                        Ok(packet) => return Some(packet),
                        Err(error) => handler.error(HandlerError::PacketValidation {
                            sender_addr: client_addr,
                            error: Box::new(error),
                        }),
                    }
                }
            },
            PacketHandling::Done { known_packet } => handler.raw_packet(RawPacket::Done {
                sender_addr: client_addr,
                known_packet,
            }),
            PacketHandling::Ack { duplicate } => handler.raw_packet(RawPacket::Ack {
                receiver_addr: client_addr,
                duplicate,
            }),
        }

        None
    }

    fn send_pings(&mut self, handler: &mut impl ServerHandler<P>) {
        for client_entry in self.clients.iter_mut() {
            let Some(client) = client_entry else { continue };

            let ping_due = client
                .active_ping
                .map(|active_ping| active_ping.ping.elapsed() >= self.config.ping_delay)
                .unwrap_or(true);
            if !ping_due {
                continue;
            }

            let sent_ping = client.active_ping.take().is_some();
            let received_pong = client.last_pong.take().is_some();
            if sent_ping && !received_pong {
                client.missed_pongs = client.missed_pongs.saturating_add(1);

                if let Some(max_missed_pongs) = self.config.max_missed_pongs {
                    if client.missed_pongs > max_missed_pongs {
                        handler.error(HandlerError::Timeout {
                            peer_addr: client.client_addr,
                        });
                        *client_entry = None;
                        continue;
                    }
                }
            }

            let ping_id = Uuid::new_v4();
            let mut buf = [0; 17];
            buf[0] = PacketKind::Ping.into();
            buf[1..17].copy_from_slice(ping_id.as_bytes());
            match NonBlockingUdpSocket::new(&self.socket, client.client_addr).send(&buf) {
                Ok(sent) => {
                    if sent {
                        client.active_ping = Some(ActivePing {
                            ping_id,
                            ping: Instant::now(),
                        });
                        handler.ping(client.client_addr);
                    }
                }
                Err(error) => {
                    handler.error(HandlerError::Send {
                        receiver_addr: client.client_addr,
                        error,
                    });
                    *client_entry = None;
                }
            }
        }
    }
}

impl<P: Packets> std::fmt::Debug for Server<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server")
            .field("config", &self.config)
            .field("socket", &self.socket)
            .field("listeners", &self.listeners)
            .field("clients", &self.clients)
            .finish()
    }
}

pub trait ServerHandler<P: Packets>: ConnectionHandler {
    /// Returns the version that should be sent to clients.
    ///
    /// This depends on the server (and is called for every new request), so that the server can
    /// send (partial) status information to clients even if they are incompatible.
    fn version(&mut self) -> P::Version;

    /// A client sent a version request, so the server is now listening to it.
    fn listen(&mut self, server_addr: SocketAddr) {
        _ = server_addr;
    }

    /// Returns the status that should be sent to clients.
    fn query(
        &mut self,
        client_addr: SocketAddr,
        query: &<P::Query as Archive>::Archived,
    ) -> P::Status;

    /// Returns whether the given client is allowed to connect (accept) or not (reject).
    fn connect(
        &mut self,
        client_addr: SocketAddr,
        connect: &<P::Connect as Archive>::Archived,
    ) -> Connect<P>;

    /// Called when a client disconnects on its own.
    fn disconnect(
        &mut self,
        client_addr: SocketAddr,
        disconnect: &<P::Disconnect as Archive>::Archived,
    ) {
        _ = (client_addr, disconnect);
    }

    /// A ping was sent to a client.
    fn ping(&mut self, client_addr: SocketAddr) {
        _ = client_addr;
    }

    /// A pong was received from a client.
    ///
    /// Pongs are only `valid` if the packet matches the one that was sent out by the server.
    fn pong(&mut self, client_addr: SocketAddr, valid: bool) {
        _ = (client_addr, valid);
    }

    /// A client sent a packet.
    fn packet(&mut self, client_addr: SocketAddr, packet: &<P::Client as Archive>::Archived) {
        _ = (client_addr, packet);
    }

    /// A client sent an unexpected packet.
    fn unexpected(&mut self, client_addr: SocketAddr, unexpected: UnexpectedClientPacket) {
        _ = (client_addr, unexpected);
    }
}

impl<P: Packets> ServerHandler<P> for DefaultConnectionHandler
where
    P::Version: Default,
    P::Status: Default,
    P::Accept: Default,
{
    fn version(&mut self) -> P::Version {
        Default::default()
    }

    fn query(
        &mut self,
        _client_addr: SocketAddr,
        _query: &<P::Query as Archive>::Archived,
    ) -> P::Status {
        Default::default()
    }

    fn connect(
        &mut self,
        _client_addr: SocketAddr,
        _connect: &<P::Connect as Archive>::Archived,
    ) -> Connect<P> {
        Connect::Accept(Default::default())
    }
}

impl<P: Packets> ServerHandler<P> for BasicLogConnectionHandler
where
    P::Version: std::fmt::Debug + Default,
    <P::Query as Archive>::Archived: std::fmt::Debug,
    P::Status: std::fmt::Debug + Default,
    <P::Connect as Archive>::Archived: std::fmt::Debug,
    P::Accept: std::fmt::Debug + Default,
    <P::Disconnect as Archive>::Archived: std::fmt::Debug,
    <P::Client as Archive>::Archived: std::fmt::Debug,
{
    fn version(&mut self) -> P::Version {
        let version = Default::default();
        println!("server version: {version:?}");
        version
    }

    fn query(
        &mut self,
        client_addr: SocketAddr,
        query: &<P::Query as Archive>::Archived,
    ) -> P::Status {
        let status = Default::default();
        println!("{client_addr} sent status query: {query:?}");
        println!("▶ responding with status: {status:?}");
        status
    }

    fn connect(
        &mut self,
        client_addr: SocketAddr,
        connect: &<P::Connect as Archive>::Archived,
    ) -> Connect<P> {
        println!("{client_addr} wants to connect with: {connect:?}");
        let accept = <P::Accept as Default>::default();
        println!("▶ accepted with: {accept:?}");
        Connect::Accept(accept)
    }

    fn disconnect(
        &mut self,
        client_addr: SocketAddr,
        disconnect: &<<P as Packets>::Disconnect as Archive>::Archived,
    ) {
        println!("{client_addr} disconnected: {disconnect:?}");
    }

    fn ping(&mut self, client_addr: SocketAddr) {
        println!("sent ping to {client_addr}");
    }

    fn pong(&mut self, client_addr: SocketAddr, valid: bool) {
        let valid = if valid { "↩ received" } else { "❌ invalid" };
        println!("{valid} pong from {client_addr}");
    }

    fn packet(
        &mut self,
        client_addr: SocketAddr,
        packet: &<<P as Packets>::Client as Archive>::Archived,
    ) {
        println!("{client_addr} sent packet: {packet:?}");
    }

    fn unexpected(&mut self, client_addr: SocketAddr, unexpected: UnexpectedClientPacket) {
        println!("{client_addr} sent unexpected packet: {unexpected}");
    }
}

pub enum Connect<P: Packets> {
    Accept(P::Accept),
    Reject(P::Reject),
}

impl<P: Packets> Clone for Connect<P>
where
    P::Accept: Clone,
    P::Reject: Clone,
{
    fn clone(&self) -> Self {
        match self {
            Self::Accept(arg0) => Self::Accept(arg0.clone()),
            Self::Reject(arg0) => Self::Reject(arg0.clone()),
        }
    }
}

impl<P: Packets> Copy for Connect<P>
where
    P::Accept: Copy,
    P::Reject: Copy,
{
}

impl<P: Packets> std::fmt::Debug for Connect<P>
where
    P::Accept: std::fmt::Debug,
    P::Reject: std::fmt::Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Accept(arg0) => f.debug_tuple("Accept").field(arg0).finish(),
            Self::Reject(arg0) => f.debug_tuple("Reject").field(arg0).finish(),
        }
    }
}

impl<P: Packets> PartialEq for Connect<P>
where
    P::Accept: PartialEq,
    P::Reject: PartialEq,
{
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Accept(l0), Self::Accept(r0)) => l0 == r0,
            (Self::Reject(l0), Self::Reject(r0)) => l0 == r0,
            _ => false,
        }
    }
}

impl<P: Packets> Eq for Connect<P>
where
    P::Accept: Eq,
    P::Reject: Eq,
{
}

#[derive(Debug, Error)]
pub enum UnexpectedClientPacket {
    #[error("client can't know if it is compatible")]
    UnknownCompatibility,
    #[error("not connected to this client")]
    NotConnected,
    #[error("already connected to this client")]
    AlreadyConnected,
}

#[derive(Debug, Error)]
#[error("cannot unlisten a connected client")]
pub struct UnlistenError;

pub struct Client<'a, P: Packets> {
    socket: &'a UdpSocket,
    client_addr: SocketAddr,
    buffers: &'a mut ServerPacketBuffers<P>,
    background_serialization_threshold: usize,
}

impl<'a, P: Packets> Client<'a, P> {
    /// Sends a packet to the client.
    pub fn send(&'a mut self, packet: P::Server) -> io::Result<PacketId> {
        self.send_raw(ServerConnectionPacket::User(packet))
    }

    fn send_raw(&'a mut self, packet: ServerConnectionPacket<P>) -> io::Result<PacketId> {
        self.buffers
            .send(packet, self.background_serialization_threshold, {
                NonBlockingUdpSocket::new(self.socket, self.client_addr)
            })
    }
}

struct ClientConnection<P: Packets> {
    client_addr: SocketAddr,
    buffers: ServerPacketBuffers<P>,
    /// The current ping sent out to the client.
    ///
    /// Might be `None` if it failed to send because the socket would have blocked.
    ///
    /// Will be kept around even after a pong was received to know when to send the next one. New
    /// pings will be sent out, even if the client hasn't responded to this one yet.
    active_ping: Option<ActivePing>,
    /// The last time the client responded to a ping.
    last_pong: Option<Instant>,
    /// How many pings the client has not responded to in a row.
    ///
    /// Reset whenever a pong is received. Used to time out clients.
    missed_pongs: usize,
    // TODO: Add a way to track which entities and things are known to be up to date:
    // updates: UpdateTracker,
}

impl<P: Packets> ClientConnection<P> {
    fn new(client_addr: SocketAddr, buffers: ServerPacketBuffers<P>) -> ClientConnection<P> {
        Self {
            client_addr,
            buffers,
            active_ping: None,
            last_pong: None,
            missed_pongs: 0,
        }
    }
}

impl<P: Packets> std::fmt::Debug for ClientConnection<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientConnection")
            .field("client_addr", &self.client_addr)
            .field("buffers", &self.buffers)
            .field("active_ping", &self.active_ping)
            .field("last_ping", &self.last_pong)
            .field("missed_pings", &self.missed_pongs)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
struct ActivePing {
    ping_id: Uuid,
    ping: Instant,
}
