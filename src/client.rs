use std::{
    io::{self, ErrorKind},
    net::{Ipv4Addr, SocketAddr, ToSocketAddrs, UdpSocket},
    time::Instant,
};

use rkyv::{check_archived_root, validation::validators::DefaultValidator, Archive, CheckBytes};

use crate::packet::{
    receive::ReceivedPacket, ArchivedServerConnectionPacket, ClientConnectionPacket,
    ClientPacketBuffers, ConnectionConfig, DefaultPackets, HandlePacketError, PacketBuffers,
    PacketHandling, PacketId, Packets, ServerConnectionPacket, PACKET_BUFFER_SIZE,
};

pub struct Client<P: Packets = DefaultPackets> {
    config: ConnectionConfig,
    socket: UdpSocket,
    buffers: ClientPacketBuffers<P>,
    state: ClientState,
    awaiting_response_since: Option<Instant>,
}

impl<P: Packets> Client<P> {
    pub fn new() -> io::Result<Self> {
        Self::with_config(Default::default())
    }

    pub fn with_config(config: ConnectionConfig) -> io::Result<Self> {
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))?;
        socket.set_nonblocking(true)?;
        Ok(Self {
            config,
            socket,
            buffers: PacketBuffers::new(config.initial_send_batch_size),
            state: ClientState::Disconnected,
            awaiting_response_since: None,
        })
    }

    pub fn query(
        &mut self,
        server_addr: impl ToSocketAddrs,
        query: P::Query,
    ) -> io::Result<PacketId> {
        self.buffers.send(
            ClientConnectionPacket::Query(query),
            self.config.background_serialization_threshold,
            |buf| self.socket.send_to(buf, &server_addr),
        )
    }

    pub fn query_broadcast(&mut self, query: P::Query) -> io::Result<PacketId> {
        self.query((Ipv4Addr::BROADCAST, 0), query)
    }

    pub fn connect(
        &mut self,
        server_addr: impl ToSocketAddrs,
        connect: P::Connect,
    ) -> io::Result<PacketId> {
        if let ClientState::Disconnected = self.state {
            self.socket.connect(server_addr)?;
            let packet_id = self.send_raw(ClientConnectionPacket::Connect(connect))?;
            self.state = ClientState::Connecting(self.socket.peer_addr()?, packet_id);
            Ok(packet_id)
        } else {
            return Err(todo!());
        }
    }

    pub fn disconnect(&mut self, disconnect: P::Disconnect) -> io::Result<PacketId> {
        if let ClientState::Connected(_) = self.state {
            let packet_id = self.send_raw(ClientConnectionPacket::Disconnect(disconnect))?;
            self.state = ClientState::Disconnecting(self.socket.peer_addr()?, packet_id);
            Ok(packet_id)
        } else {
            return Err(todo!());
        }
    }

    pub fn send(&mut self, packet: P::Client) -> io::Result<PacketId> {
        self.send_raw(ClientConnectionPacket::User(packet))
    }

    pub fn update(&mut self) -> io::Result<()> {
        self.buffers
            .update(self.config.timeout, self.config.resend_delay, |buf| {
                self.socket.send(buf)
            })
    }

    pub fn handle(&mut self, handler: Handler<P>) -> Result<(), HandlePacketError>
    where
        <ServerConnectionPacket<P> as Archive>::Archived: for<'a> CheckBytes<DefaultValidator<'a>>,
    {
        let mut buf = [0; PACKET_BUFFER_SIZE];
        loop {
            let (buf, server_addr) = match self.socket.recv_from(&mut buf) {
                Ok((size, server_addr)) => (&buf[..size], server_addr),
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    return Ok(());
                }
                Err(error) => Err(error)?,
            };

            self.awaiting_response_since = Some(Instant::now());

            match self.buffers.handle(buf, |buf| self.socket.send(buf))? {
                PacketHandling::Received(ack) => match ack {
                    ReceivedPacket::Pending { duplicate } => {
                        (handler.on_pending)(duplicate);
                    }
                    ReceivedPacket::Reassembled(bytes) => {
                        let packet = check_archived_root::<ServerConnectionPacket<P>>(bytes)
                            .map_err(|_| HandlePacketError::Validation)?;
                        match packet {
                            ArchivedServerConnectionPacket::Status(status) => {
                                (handler.on_status)(server_addr, status);
                            }
                            ArchivedServerConnectionPacket::Accept(accept) => {
                                if let ClientState::Connecting(expected_server_addr, packet_id) =
                                    self.state
                                {
                                    if server_addr == expected_server_addr {
                                        (handler.on_accept)(accept);
                                        self.state = ClientState::Connected(server_addr);
                                        continue;
                                    }
                                }
                                (handler.on_unexpected)(server_addr, Unexpected::Accept(accept));
                            }
                            ArchivedServerConnectionPacket::Reject(reject) => {
                                if let ClientState::Connecting(expected_server_addr, packet_id) =
                                    self.state
                                {
                                    if server_addr == expected_server_addr {
                                        (handler.on_reject)(reject);
                                        self.state = ClientState::Disconnected;
                                        continue;
                                    }
                                }
                                (handler.on_unexpected)(server_addr, Unexpected::Reject(reject));
                            }
                            ArchivedServerConnectionPacket::Kick(kick) => {
                                if let ClientState::Connected(expected_server_addr) = self.state {
                                    if server_addr == expected_server_addr {
                                        (handler.on_kick)(kick);
                                        self.state = ClientState::Disconnected;
                                        continue;
                                    }
                                }
                                (handler.on_unexpected)(server_addr, Unexpected::Kick(kick));
                            }
                            ArchivedServerConnectionPacket::User(packet) => {
                                if let ClientState::Connected(expected_server_addr) = self.state {
                                    if server_addr == expected_server_addr {
                                        (handler.on_packet)(packet);
                                        continue;
                                    }
                                }
                                (handler.on_unexpected)(server_addr, Unexpected::Packet(packet));
                            }
                        }
                    }
                },
                PacketHandling::Done => {
                    (&handler.on_done)();
                }
                PacketHandling::Ack { duplicate } => {
                    (&handler.on_ack)(duplicate);
                }
            }
        }
    }

    fn send_raw(&mut self, packet: ClientConnectionPacket<P>) -> Result<PacketId, io::Error> {
        self.buffers.send(
            packet,
            self.config.background_serialization_threshold,
            |buf| self.socket.send(buf),
        )
    }
}

// TODO: Do I really nned PacketId?
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClientState {
    /// Client is not connected.
    Disconnected,
    /// Client is awaiting acceptance by the server.
    Connecting(SocketAddr, PacketId),
    /// Client was accepted by the server and is connected.
    Connected(SocketAddr),
    /// Client is waiting for the server to acknowledge it has disconnected.
    Disconnecting(SocketAddr, PacketId),
}

pub struct Handler<'a, P: Packets> {
    pub on_status: &'a dyn Fn(SocketAddr, &<P::Status as Archive>::Archived),
    pub on_accept: &'a dyn Fn(&<P::Accept as Archive>::Archived),
    pub on_reject: &'a dyn Fn(&<P::Reject as Archive>::Archived),
    pub on_kick: &'a dyn Fn(&<P::Kick as Archive>::Archived),
    pub on_packet: &'a dyn Fn(&<P::Server as Archive>::Archived),

    pub on_pending: &'a dyn Fn(bool),
    pub on_done: &'a dyn Fn(),
    pub on_ack: &'a dyn Fn(bool),

    pub on_unexpected: &'a dyn Fn(SocketAddr, Unexpected<P>),
}

pub enum Unexpected<'a, P: Packets> {
    Accept(&'a <P::Accept as Archive>::Archived),
    Reject(&'a <P::Reject as Archive>::Archived),
    Kick(&'a <P::Kick as Archive>::Archived),
    Packet(&'a <P::Server as Archive>::Archived),
}

// TODO: disconnect on drop if the client is connected
//       ... which requires moving the client to a separate thread to let the disconnect packet go through

// impl Drop for Client {
//     fn drop(&mut self) {
//         // try to notify the server, but it's fine if it fails
//         self.socket.send(&Disconnect.serialize()).ok();
//     }
// }
