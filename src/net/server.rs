use std::{
    io::{self, ErrorKind},
    net::{SocketAddr, ToSocketAddrs, UdpSocket},
    time::Instant,
};

use rkyv::{check_archived_root, validation::validators::DefaultValidator, Archive, CheckBytes};
use thiserror::Error;
use uuid::Uuid;

use crate::packet::{
    receive::ReceivedPacket, ArchivedClientConnectionPacket, ClientConnectionPacket,
    DefaultPackets, NonBlocking, PacketBuffers, PacketHandling, PacketId, PacketKind, Packets,
    ServerConfig, ServerConnectionPacket, ServerPacketBuffers, VersionPacket, PACKET_BUFFER_SIZE,
};

use super::{
    BasicLogConnectionHandler, ConnectionHandler, DefaultConnectionHandler, HandlerError,
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
    /// after [`ServerConfig::listener_timeout`].
    listeners: Vec<ClientListener<P>>,
    /// All clients that are connected to the server.
    ///
    /// Clients get moved from `listeners` to this list when they try to connect. Connected clients
    /// cannot move position, since the order is also used by entity change tracking. Instead, they
    /// are simply set to [`None`] upon disconnection and can be reused by new connections.
    connections: Vec<Option<ConnectedClient<P>>>,
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
            connections: Default::default(),
        })
    }

    pub fn client(&mut self, client_addr: SocketAddr) -> Option<Client<P>> {
        self.listeners
            .iter_mut()
            .flatten()
            .find(|connection| connection.client_addr == client_addr)
            .and_then(|connection| connection.client.as_mut())
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
        for connection_entry in self.listeners.iter_mut() {
            let Some(connection) = connection_entry else {
                continue;
            };
            let result = connection.buffers.update(
                self.config.connection.timeout,
                self.config.connection.resend_delay,
                NonBlockingUdpSocket::new(&self.socket, connection.client_addr),
            );
            match result {
                Ok(()) => {}
                Err(error) => {
                    if error.kind() == ErrorKind::WouldBlock {
                        // give the socket some time to empty its buffer
                        break;
                    }
                    handler.error(HandlerError::Send {
                        receiver_addr: connection.client_addr,
                        error,
                    });
                    *connection_entry = None;
                }
            }
        }

        let mut packet_buf = [0; PACKET_BUFFER_SIZE];
        loop {
            let (buf, client_addr) = match self.socket.recv_from(&mut packet_buf) {
                Ok((size, addr)) => (&packet_buf[..size], addr),
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    break;
                }
                Err(error) => {
                    handler.error(HandlerError::Recv(error));
                    break;
                }
            };

            let is_version_request = buf.is_empty();

            let connection_entry = self.listeners.iter_mut().find(|connection| {
                connection
                    .as_ref()
                    .is_some_and(|connection| connection.client_addr == client_addr)
            });

            let Some(connection_entry) = connection_entry else {
                if !is_version_request {
                    handler.unexpected(client_addr, UnexpectedClientPacket::UnknownCompatibility);
                    continue;
                }

                match Self::send_version(&self.socket, &mut packet_buf, handler, client_addr) {
                    Ok(sent) => {
                        if !sent {
                            continue;
                        }
                        let now = Instant::now();
                        self.listeners.push(Some(ClientListener {
                            connected: false,
                            client_addr,
                            ping_id: Uuid::new_v4(),
                            ping: now,
                            pong: None,
                            buffers: PacketBuffers::new(
                                self.config.connection.initial_send_batch_size,
                            ),
                        }));
                    }
                    Err(error) => {
                        handler.error(HandlerError::Send {
                            receiver_addr: client_addr,
                            error,
                        });
                    }
                }
                continue;
            };

            let connection = connection_entry.as_mut().unwrap();

            if is_version_request {
                if let Err(error) =
                    Self::send_version(&self.socket, &mut packet_buf, handler, client_addr)
                {
                    handler.error(HandlerError::Send {
                        receiver_addr: client_addr,
                        error,
                    });
                }
                // just try again the next time if it couldn't be sent
                continue;
            }

            let packet_handling = connection
                .buffers
                .handle(buf, NonBlockingUdpSocket::new(&self.socket, client_addr));

            let packet_handling = match packet_handling {
                Ok(packet_handling) => packet_handling,
                Err(error) => {
                    handler.error(HandlerError::Handle {
                        sender_addr: client_addr,
                        error,
                    });
                    continue;
                }
            };

            match packet_handling {
                PacketHandling::Received(ack) => match ack {
                    ReceivedPacket::Pending => {
                        handler.raw_packet(RawPacket::Part {
                            sender_addr: client_addr,
                            info: PartInfo::Pending,
                        });
                    }
                    ReceivedPacket::Duplicate => {
                        handler.raw_packet(RawPacket::Part {
                            sender_addr: client_addr,
                            info: PartInfo::Duplicate,
                        });
                    }
                    ReceivedPacket::Reassembled(bytes) => {
                        handler.raw_packet(RawPacket::Part {
                            sender_addr: client_addr,
                            info: PartInfo::Reassembled,
                        });

                        let packet = check_archived_root::<ClientConnectionPacket<P>>(bytes);
                        let packet = match packet {
                            Ok(packet) => packet,
                            Err(error) => {
                                handler.error(HandlerError::PacketValidation {
                                    sender_addr: client_addr,
                                    error: Box::new(error),
                                });
                                continue;
                            }
                        };
                        match packet {
                            ArchivedClientConnectionPacket::Query(query) => {
                                let status = handler.query(client_addr, query);
                                let result = connection.buffers.send(
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
                                if !connection.connected {
                                    *connection_entry = None;
                                }
                            }
                            ArchivedClientConnectionPacket::Connect(connect) => {
                                if connection.connected {
                                    handler.unexpected(
                                        client_addr,
                                        UnexpectedClientPacket::AlreadyConnected,
                                    );
                                } else {
                                    match handler.connect(client_addr, connect) {
                                        Connect::Accept(accept) => {
                                            let result = connection.buffers.send(
                                                ServerConnectionPacket::Accept(accept),
                                                self.config
                                                    .connection
                                                    .background_serialization_threshold,
                                                NonBlockingUdpSocket::new(
                                                    &self.socket,
                                                    client_addr,
                                                ),
                                            );
                                            if let Err(error) = result {
                                                handler.error(HandlerError::Send {
                                                    receiver_addr: client_addr,
                                                    error,
                                                })
                                            }
                                            connection.connected = true;
                                        }
                                        Connect::Reject(reject) => {
                                            let result = connection.buffers.send(
                                                ServerConnectionPacket::Reject(reject),
                                                self.config
                                                    .connection
                                                    .background_serialization_threshold,
                                                NonBlockingUdpSocket::new(
                                                    &self.socket,
                                                    client_addr,
                                                ),
                                            );
                                            if let Err(error) = result {
                                                handler.error(HandlerError::Send {
                                                    receiver_addr: client_addr,
                                                    error,
                                                })
                                            }
                                            *connection_entry = None;
                                        }
                                    }
                                }
                            }
                            ArchivedClientConnectionPacket::Disconnect(disconnect) => {
                                if connection.connected {
                                    handler.disconnect(client_addr, disconnect);
                                } else {
                                    handler.unexpected(
                                        client_addr,
                                        UnexpectedClientPacket::NotConnected,
                                    );
                                }
                                *connection_entry = None;
                            }
                            ArchivedClientConnectionPacket::User(packet) => {
                                if connection.connected {
                                    handler.packet(client_addr, packet);
                                } else {
                                    handler.unexpected(
                                        client_addr,
                                        UnexpectedClientPacket::NotConnected,
                                    )
                                }
                            }
                        }
                    }
                },
                PacketHandling::Done { known_packet } => {
                    handler.raw_packet(RawPacket::Done {
                        sender_addr: client_addr,
                        known_packet,
                    });
                }
                PacketHandling::Ack { duplicate } => {
                    handler.raw_packet(RawPacket::Ack {
                        receiver_addr: client_addr,
                        duplicate,
                    });
                }
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
}

impl<P: Packets> std::fmt::Debug for Server<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server")
            .field("config", &self.config)
            .field("socket", &self.socket)
            .field("connections", &self.listeners)
            .finish()
    }
}

pub trait ServerHandler<P: Packets>: ConnectionHandler {
    /// Returns the version that should be sent to clients.
    ///
    /// This depends on the server (and is called for every new request), so that the server can
    /// send (partial) status information to clients even if they are incompatible.
    fn version(&mut self) -> P::Version;

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
        disconect: &<P::Disconnect as Archive>::Archived,
    ) {
        _ = (client_addr, disconect);
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
    P::Accept: Default,
    Connect<P>: std::fmt::Debug,
{
    fn version(&mut self) -> P::Version {
        let version = Default::default();
        println!("{version:?}");
        version
    }

    fn query(
        &mut self,
        client_addr: SocketAddr,
        query: &<P::Query as Archive>::Archived,
    ) -> P::Status {
        let status = Default::default();
        println!("{client_addr} {query:?} {status:?}");
        status
    }

    fn connect(
        &mut self,
        client_addr: SocketAddr,
        connect: &<P::Connect as Archive>::Archived,
    ) -> Connect<P> {
        let accept = Connect::Accept(Default::default());
        println!("{client_addr} {connect:?} {accept:?}");
        accept
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
    #[error("incompatible with this client")]
    Incompatible,
    #[error("already connected to this client")]
    AlreadyConnected,
}

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

struct ClientListener<P: Packets> {
    client_addr: SocketAddr,
    client: Option<ConnectedClient<P>>,
}

impl<P: Packets> std::fmt::Debug for ClientListener<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientListener")
            .field("client_addr", &self.client_addr)
            .field("client", &self.client)
            .finish()
    }
}

struct ConnectedClient<P: Packets> {
    client_addr: SocketAddr,
    ping_id: Uuid,
    ping: Instant,
    pong: Option<Instant>,
    buffers: ServerPacketBuffers<P>,
    // TODO: Add a way to track which entities and things are known to be up to date:
    // updates: UpdateTracker,
}

impl<P: Packets> std::fmt::Debug for ConnectedClient<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectedClient")
            .field("ping_id", &self.ping_id)
            .field("ping", &self.ping)
            .field("pong", &self.pong)
            .field("buffers", &self.buffers)
            .finish()
    }
}
