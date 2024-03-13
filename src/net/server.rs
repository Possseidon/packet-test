use std::{
    io::{self, ErrorKind},
    net::{SocketAddr, ToSocketAddrs, UdpSocket},
    time::Instant,
};

use rkyv::{check_archived_root, validation::validators::DefaultValidator, Archive, CheckBytes};
use uuid::Uuid;

use crate::packet::{
    receive::ReceivedPacket, ArchivedClientConnectionPacket, ClientConnectionPacket,
    DefaultPackets, NonBlocking, PacketBuffers, PacketHandling, PacketId, PacketKind, Packets,
    ServerConfig, ServerConnectionPacket, ServerPacketBuffers, VersionPacket, PACKET_BUFFER_SIZE,
};

use super::{ConnectionHandler, DefaultConnectionHandler, HandlerError, NonBlockingUdpSocket};

pub struct Server<P: Packets = DefaultPackets> {
    config: ServerConfig,
    socket: UdpSocket,
    /// All connections to clients.
    ///
    /// A client cannot move position, since the order is also used by entity change tracking.
    connections: Vec<Option<Connection<P>>>,
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
            connections: Default::default(),
        })
    }

    pub fn client(&mut self, client_addr: SocketAddr) -> Option<Client<P>> {
        self.connections
            .iter_mut()
            .flatten()
            .find(|connection| connection.client_addr == client_addr)
            .map(|connection| Client {
                socket: &self.socket,
                connection,
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
        for connection_entry in self.connections.iter_mut() {
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
                        peer_addr: connection.client_addr,
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

            let connection_entry = self.connections.iter_mut().find(|connection| {
                connection
                    .as_ref()
                    .is_some_and(|connection| connection.client_addr == client_addr)
            });

            let connection_entry = if let Some(connection_entry) = connection_entry {
                connection_entry
            } else {
                let now = Instant::now();
                self.connections.push(Some(Connection {
                    connected: false,
                    client_addr,
                    ping_id: Uuid::new_v4(),
                    ping: now,
                    pong: None,
                    buffers: PacketBuffers::new(self.config.connection.initial_send_batch_size),
                }));
                self.connections.last_mut().unwrap()
            };

            if buf.is_empty() {
                // empty packet corresponds to a version query
                packet_buf[0] = PacketKind::Version.into();
                let version_len = handler.version(client_addr).write(&mut packet_buf[1..]);
                let buf = &packet_buf[..version_len + 1];
                if let Err(error) = NonBlockingUdpSocket::new(&self.socket, client_addr).send(buf) {
                    handler.error(HandlerError::Send {
                        peer_addr: client_addr,
                        error,
                    });
                }
                // ignore if it wasn't sent because the socket would have blocked
                continue;
            }

            let connection = connection_entry.as_mut().unwrap();

            let packet_handling = connection
                .buffers
                .handle(buf, NonBlockingUdpSocket::new(&self.socket, client_addr));

            let packet_handling = match packet_handling {
                Ok(packet_handling) => packet_handling,
                Err(error) => {
                    handler.error(HandlerError::Handle {
                        peer_addr: client_addr,
                        error,
                    });
                    continue;
                }
            };

            match packet_handling {
                PacketHandling::Received(ack) => match ack {
                    ReceivedPacket::Pending { duplicate } => {
                        handler.pending(client_addr, duplicate);
                    }
                    ReceivedPacket::Reassembled(bytes) => {
                        let packet = check_archived_root::<ClientConnectionPacket<P>>(bytes);
                        let packet = match packet {
                            Ok(packet) => packet,
                            Err(error) => {
                                handler.error(HandlerError::PacketValidation {
                                    peer_addr: client_addr,
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
                                        peer_addr: client_addr,
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
                                        UnexpectedClientPacket::Connect(connect),
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
                                                    peer_addr: client_addr,
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
                                                    peer_addr: client_addr,
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
                                        UnexpectedClientPacket::Disconnect(disconnect),
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
                                        UnexpectedClientPacket::Packet(packet),
                                    )
                                }
                            }
                        }
                    }
                },
                PacketHandling::Done => {
                    handler.done(client_addr);
                }
                PacketHandling::Ack { duplicate } => {
                    handler.ack(client_addr, duplicate);
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
            .field("connections", &self.connections)
            .finish()
    }
}

pub trait ServerHandler<P: Packets>: ConnectionHandler {
    fn version(&mut self, _client_addr: SocketAddr) -> P::Version;

    fn query(
        &mut self,
        _client_addr: SocketAddr,
        _query: &<P::Query as Archive>::Archived,
    ) -> P::Status;
    fn connect(
        &mut self,
        _client_addr: SocketAddr,
        _connect: &<P::Connect as Archive>::Archived,
    ) -> Connect<P>;
    fn disconnect(
        &mut self,
        _client_addr: SocketAddr,
        _disconect: &<P::Disconnect as Archive>::Archived,
    ) {
    }
    fn packet(&mut self, _client_addr: SocketAddr, _packet: &<P::Client as Archive>::Archived) {}

    fn unexpected(&mut self, _client_addr: SocketAddr, _unexpected: UnexpectedClientPacket<P>) {}
}

impl<P: Packets> ServerHandler<P> for DefaultConnectionHandler
where
    <P as Packets>::Version: Default,
    <P as Packets>::Status: Default,
    <P as Packets>::Accept: Default,
{
    fn version(&mut self, _client_addr: SocketAddr) -> <P as Packets>::Version {
        Default::default()
    }

    fn query(
        &mut self,
        _client_addr: SocketAddr,
        _query: &<<P as Packets>::Query as Archive>::Archived,
    ) -> <P as Packets>::Status {
        Default::default()
    }

    fn connect(
        &mut self,
        _client_addr: SocketAddr,
        _connect: &<<P as Packets>::Connect as Archive>::Archived,
    ) -> Connect<P> {
        Connect::Accept(Default::default())
    }
}

pub enum Connect<P: Packets> {
    Accept(P::Accept),
    Reject(P::Reject),
}

pub enum UnexpectedClientPacket<'a, P: Packets> {
    Connect(&'a <P::Connect as Archive>::Archived),
    Disconnect(&'a <P::Disconnect as Archive>::Archived),
    Packet(&'a <P::Client as Archive>::Archived),
}

pub struct Client<'a, P: Packets> {
    socket: &'a UdpSocket,
    connection: &'a mut Connection<P>,
    background_serialization_threshold: usize,
}

impl<'a, P: Packets> Client<'a, P> {
    /// Sends a packet to the client.
    pub fn send(&'a mut self, packet: P::Server) -> io::Result<PacketId> {
        self.send_raw(ServerConnectionPacket::User(packet))
    }

    fn send_raw(&'a mut self, packet: ServerConnectionPacket<P>) -> io::Result<PacketId> {
        self.connection
            .buffers
            .send(packet, self.background_serialization_threshold, {
                NonBlockingUdpSocket::new(self.socket, self.connection.client_addr)
            })
    }

    // /// Kicks the client.
    // pub fn kick(&'a mut self) -> io::Result<PacketId> {
    //     self.connection
    //         .buffers
    //         .send(ServerConnectionPacket::Kick, 1, {
    //             |buf| self.socket.send_to(buf, self.connection.addr)
    //         })
    // }
}

struct Connection<P: Packets> {
    /// Connections
    connected: bool,
    client_addr: SocketAddr,
    ping_id: Uuid,
    ping: Instant,
    pong: Option<Instant>,
    buffers: ServerPacketBuffers<P>,
    // TODO: Add a way to track which entities and things are known to be up to date:
    // updates: UpdateTracker,
}

// manual impl, since P doesn't have to be Debug
impl<P: Packets> std::fmt::Debug for Connection<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection")
            .field("client_addr", &self.client_addr)
            .field("ping_id", &self.ping_id)
            .field("ping", &self.ping)
            .field("pong", &self.pong)
            .field("buffers", &self.buffers)
            .finish()
    }
}
