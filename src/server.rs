use std::{
    io::{self, ErrorKind},
    net::{SocketAddr, ToSocketAddrs, UdpSocket},
    time::Instant,
};

use rkyv::{check_archived_root, validation::validators::DefaultValidator, Archive, CheckBytes};
use uuid::Uuid;

use crate::packet::{
    receive::ReceivedPacket, ArchivedClientConnectionPacket, ClientConnectionPacket,
    ConnectionConfig, DefaultPackets, HandlePacketError, PacketBuffers, PacketHandling, PacketId,
    Packets, ServerConfig, ServerConnectionPacket, ServerPacketBuffers, PACKET_BUFFER_SIZE,
};

pub struct Server<P: Packets = DefaultPackets> {
    config: ConnectionConfig,
    server_config: ServerConfig,
    socket: UdpSocket,
    /// All connections to clients.
    ///
    /// A client cannot move position, since the order is also used by entity change tracking.
    connections: Vec<Option<Connection<P>>>,
}

impl<P: Packets> Server<P> {
    pub fn host(addr: impl ToSocketAddrs) -> io::Result<Self> {
        Self::host_with_config(addr, Default::default(), Default::default())
    }

    pub fn host_with_config(
        addr: impl ToSocketAddrs,
        config: ConnectionConfig,
        server_config: ServerConfig,
    ) -> io::Result<Self> {
        let socket = UdpSocket::bind(addr)?;
        socket.set_nonblocking(true)?;
        Ok(Self {
            config,
            server_config,
            socket,
            connections: Default::default(),
        })
    }

    pub fn client(&mut self, addr: SocketAddr) -> Option<Client<P>> {
        self.connections
            .iter_mut()
            .flatten()
            .find(|connection| connection.addr == addr)
            .map(|connection| Client {
                socket: &self.socket,
                connection,
                background_serialization_threshold: self.config.background_serialization_threshold,
            })
    }

    pub fn update(&mut self) -> io::Result<()> {
        // TODO: Handle errors here directly?
        //       Currently there is no way to find out which errored and it also stop the loop on the first error
        for connection in self.connections.iter_mut().flatten() {
            connection
                .buffers
                .update(self.config.timeout, self.config.resend_delay, |buf| {
                    self.socket.send(buf)
                })?;
        }
        Ok(())
    }

    pub fn handle(&mut self, handler: Handler<P>) -> Result<(), HandlePacketError>
    where
        <ClientConnectionPacket<P> as Archive>::Archived: for<'a> CheckBytes<DefaultValidator<'a>>,
    {
        Ok(())
        // let mut buf = [0; PACKET_BUFFER_SIZE];
        // loop {
        //     let (buf, client_addr) = match self.socket.recv_from(&mut buf) {
        //         Ok((size, addr)) => (&buf[..size], addr),
        //         Err(error) if error.kind() == ErrorKind::WouldBlock => {
        //             return Ok(());
        //         }
        //         Err(error) => Err(error)?,
        //     };

        //     let connection_entry = self.connections.iter_mut().find(|connection| {
        //         connection.is_some_and(|connection| connection.addr == client_addr)
        //     });

        //     let connection_entry = if let Some(connection_entry) = connection_entry {
        //         connection_entry
        //     } else {
        //         let now = Instant::now();
        //         self.connections.push(Some(Connection {
        //             connected: false,
        //             addr: client_addr,
        //             ping_id: Uuid::new_v4(),
        //             ping: now,
        //             pong: None,
        //             buffers: PacketBuffers::new(self.config.initial_send_batch_size),
        //         }));
        //         self.connections.last_mut().unwrap()
        //     };

        //     let connection = connection_entry.as_mut().unwrap();

        //     let mut client = Client {
        //         socket: &self.socket,
        //         background_serialization_threshold: self.config.background_serialization_threshold,
        //         connection,
        //     };

        //     match connection
        //         .buffers
        //         .handle(buf, |buf| self.socket.send_to(buf, client_addr))?
        //     {
        //         PacketHandling::Received(ack) => match ack {
        //             ReceivedPacket::Pending { duplicate } => {
        //                 (handler.on_pending)(client_addr, duplicate);
        //             }
        //             ReceivedPacket::Reassembled(bytes) => {
        //                 let packet = check_archived_root::<ClientConnectionPacket<P>>(bytes)
        //                     .map_err(|_| HandlePacketError::Validation)?;
        //                 match packet {
        //                     ArchivedClientConnectionPacket::Query(query) => {
        //                         client.send_raw(ServerConnectionPacket::Status((handler
        //                             .on_query)(
        //                             client_addr,
        //                             query,
        //                         )))?;
        //                         if !connection.connected {
        //                             *connection_entry = None;
        //                         }
        //                     }
        //                     ArchivedClientConnectionPacket::Connect(connect) => {
        //                         if connection.connected {
        //                             (handler.on_unexpected)(
        //                                 client_addr,
        //                                 Unexpected::Connect(connect),
        //                             );
        //                         } else {
        //                             match (handler.on_connect)(client_addr, connect) {
        //                                 Connect::Accept(accept) => {
        //                                     client.send_raw(ServerConnectionPacket::Accept(accept));
        //                                     connection.connected = true;
        //                                 }
        //                                 Connect::Reject(reject) => {
        //                                     client.send_raw(ServerConnectionPacket::Reject(reject));
        //                                     *connection_entry = None;
        //                                 }
        //                             }
        //                         }
        //                     }
        //                     ArchivedClientConnectionPacket::Disconnect(disconnect) => {
        //                         if connection.connected {
        //                             (handler.on_disconnect)(client_addr, disconnect);
        //                         } else {
        //                             (handler.on_unexpected)(
        //                                 client_addr,
        //                                 Unexpected::Disconnect(disconnect),
        //                             );
        //                         }
        //                         *connection_entry = None;
        //                     }
        //                     ArchivedClientConnectionPacket::User(packet) => {
        //                         if connection.connected {
        //                             (handler.on_packet)(client_addr, packet);
        //                         } else {
        //                             (handler.on_unexpected)(client_addr, Unexpected::Packet(packet))
        //                         }
        //                     }
        //                 }
        //             }
        //         },
        //         PacketHandling::Done => {
        //             (handler.on_done)(client_addr);
        //         }
        //         PacketHandling::Ack { duplicate } => {
        //             (handler.on_ack)(client_addr, duplicate);
        //         }
        //     }
        // }
    }
}

impl<P: Packets> std::fmt::Debug for Server<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server")
            .field("config", &self.config)
            .field("server_config", &self.server_config)
            .field("socket", &self.socket)
            .field("connections", &self.connections)
            .finish()
    }
}

pub struct Handler<'a, P: Packets> {
    pub on_query: &'a dyn Fn(SocketAddr, &<P::Query as Archive>::Archived) -> P::Status,
    pub on_connect: &'a dyn Fn(SocketAddr, &<P::Connect as Archive>::Archived) -> Connect<P>,
    pub on_disconnect: &'a dyn Fn(SocketAddr, &<P::Disconnect as Archive>::Archived),

    pub on_packet: &'a dyn Fn(SocketAddr, &<P::Client as Archive>::Archived),

    pub on_pending: &'a dyn Fn(SocketAddr, bool),
    pub on_done: &'a dyn Fn(SocketAddr),
    pub on_ack: &'a dyn Fn(SocketAddr, bool),

    pub on_unexpected: &'a dyn Fn(SocketAddr, Unexpected<'a, P>),
}

pub enum Connect<P: Packets> {
    Accept(P::Accept),
    Reject(P::Reject),
}

pub enum Unexpected<'a, P: Packets> {
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
                |buf| self.socket.send_to(buf, self.connection.addr)
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
    addr: SocketAddr,
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
            .field("addr", &self.addr)
            .field("ping_id", &self.ping_id)
            .field("ping", &self.ping)
            .field("pong", &self.pong)
            .field("buffers", &self.buffers)
            .finish()
    }
}
