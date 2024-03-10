use std::{
    io,
    net::{Ipv4Addr, SocketAddr, UdpSocket},
    time::{Duration, Instant},
};

use uuid::Uuid;

use crate::packet::{NonZeroBatchSize, PacketBuffers, Packets};

// TODO: Make ServerPacket and ClientPacket generic
pub struct Server<P: Packets> {
    socket: UdpSocket,
    /// How long a client has time to respond to a ping before getting disconnected.
    timeout: Duration,
    /// How long to wait until sending the next ping after a client has ponged.
    ping_delay: Duration,
    /// All connections to clients.
    ///
    /// A client cannot move position, since the order is also used by entity change tracking.
    connections: Vec<Option<Connection<P>>>,
}

impl<P: Packets> std::fmt::Debug for Server<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server")
            .field("socket", &self.socket)
            .field("timeout", &self.timeout)
            .field("ping_delay", &self.ping_delay)
            .field("connections", &self.connections)
            .finish()
    }
}

impl<P: Packets> Server<P> {
    pub const DEFAULT_PORT: u16 = 42069;

    pub fn new() -> io::Result<Self> {
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, Self::DEFAULT_PORT))?;
        socket.set_nonblocking(true)?;
        Ok(Self {
            socket,
            timeout: Duration::from_secs(10),
            ping_delay: Duration::from_secs(3),
            connections: Default::default(),
        })
    }

    // fn connect(&mut self, addr: SocketAddr) -> Result<(), AlreadyConnected> {
    //     if self.connection_mut(addr).is_ok() {
    //         return Err(AlreadyConnected(addr));
    //     }

    //     let send = |buf| self.socket.send_to(buf, addr);

    //     self.socket.send_to(&Accept.serialize(), addr);

    //     let ping_uuid = Uuid::new_v4();
    //     let connection = Some(Connection {
    //         addr,
    //         ping_uuid,
    //         ping: Instant::now(),
    //         pong: None,
    //         batch_size: NonZeroBatchSize::new(16).unwrap(),
    //         send_buffer: default(),
    //         receive_buffer: default(),
    //     });

    //     self.socket.send_to(&Ping(ping_uuid).serialize(), addr);

    //     if let Some(pos) = self.connections.iter().position(Option::is_none) {
    //         self.connections[pos] = connection;
    //     } else {
    //         self.connections.push(connection);
    //     };
    //     Ok(())
    // }

    // fn connection_mut(&mut self, addr: SocketAddr) -> Result<&mut Connection, NotConnected> {
    //     self.connections
    //         .iter_mut()
    //         .flatten()
    //         .find(|connection| connection.addr == addr)
    //         .ok_or(NotConnected(addr))
    // }

    // fn disconnect(&mut self, addr: SocketAddr) -> Result<(), NotConnected> {
    //     let connection = self
    //         .connections
    //         .iter_mut()
    //         .find(|connection| connection.is_some_and(|connection| connection.addr == addr));
    //     if let Some(connection) = connection {
    //         // TODO: Go through all entities and clear this client, so that it is clean for a new connection
    //         *connection = None;
    //         Ok(())
    //     } else {
    //         Err(NotConnected(addr))
    //     }
    // }

    // fn handle_client_packet(&mut self, packet: ClientPacket, addr: SocketAddr) {
    //     match packet {
    //         ClientPacket::Pong(uuid) => {
    //             if let Ok(connection) = self.connection_mut(addr) {
    //                 if connection.ping_uuid == uuid {
    //                     connection.pong = Some(Instant::now());
    //                     info!("{addr} ponged after {:?}", connection.ping.elapsed());
    //                 } else {
    //                     warn!(
    //                         "{addr}: ignored pong with incorrect UUID ({} expected, got {uuid})",
    //                         connection.ping_uuid
    //                     );
    //                 }
    //             } else {
    //                 warn!("{addr}: ignored pong from unknown client");
    //             }
    //         }
    //         ClientPacket::Connect => match self.connect(addr) {
    //             Ok(()) => {
    //                 info!("{addr} connected");
    //             }
    //             Err(AlreadyConnected(_)) => {
    //                 info!("{addr}: ignored connect from already connected client");
    //             }
    //         },
    //         ClientPacket::Disconnect => match self.disconnect(addr) {
    //             Ok(()) => {
    //                 info!("{addr} disconnected");
    //             }
    //             Err(NotConnected(_)) => {
    //                 info!("{addr}: ignored disconnect from unknown client");
    //             }
    //         },
    //     }
    // }

    // /// Disconnects any clients that haven't ponged in a while.
    // fn timeout_clients(&mut self) {
    //     for connection_entry in &mut self.connections {
    //         if let Some(connection) = connection_entry {
    //             if connection.pong.is_none() && connection.ping.elapsed() > self.timeout {
    //                 warn!("{:?} timed out", connection.addr);
    //                 // notify the client, in case it is still listening and just can't respond
    //                 self.send(&Kick.serialize(), connection.addr).ok();
    //                 *connection_entry = None;
    //             }
    //         }
    //     }
    // }

    // /// Pings clients that have not been pinged in a while.
    // fn ping_clients(&mut self) {
    //     for connection in self.connections.iter_mut().flatten() {
    //         if let Some(pong) = connection.pong {
    //             if pong.elapsed() > self.ping_delay {
    //                 connection.ping = Instant::now();
    //                 connection.ping_uuid = Uuid::new_v4();
    //                 connection.pong = None;

    //                 self.update(connection.addr, &Ping(connection.ping_uuid));
    //             }
    //         }
    //     }
    // }

    // /// Sends a reliable packet to the given client and returns an id that can be used to track it.
    // fn send(&self, addr: SocketAddr, packet: &ServerPacket) -> Result<Uuid, ()> {
    //     if let Ok(connection) = self.connection_mut(addr) {
    //         Ok(connection.send_buffer.send(
    //             packet.payload(),
    //             |buf| self.socket.send_to(buf, addr),
    //             connection.batch_size,
    //         ))
    //     } else {
    //         todo!("client no longer connected");
    //         Err(())
    //     }
    // }

    // /// Sends an unreliable update packet to the given client.
    // fn update(&self, addr: SocketAddr, packet: impl ServerUpdate) -> io::Result<()> {
    //     self.update_many(addr, once(packet))?;
    //     Ok(())
    // }

    // /// Sends a bunch of unreliable update packets to the given client.
    // ///
    // /// This is more efficient than sending one at a time, since it can reuse the same buffer.
    // fn update_many(
    //     &self,
    //     addr: SocketAddr,
    //     packets: impl Iterator<Item = impl ServerUpdate>,
    // ) -> io::Result<()> {
    //     let mut buf = [0; PACKET_BUFFER_SIZE];
    //     for packet in packets {
    //         self.socket.send_to(packet.payload(&mut buf), addr)?;
    //     }
    //     Ok(())
    // }
}

struct Connection<P: Packets> {
    addr: SocketAddr,
    ping_id: Uuid,
    ping: Instant,
    pong: Option<Instant>,
    batch_size: NonZeroBatchSize,
    buffers: PacketBuffers<P::Server, P::Client>,
    // TODO: Add a way to track which entities and things are known to be up to date:
    // updates: UpdateTracker,
}

impl<P: Packets> std::fmt::Debug for Connection<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection")
            .field("addr", &self.addr)
            .field("ping_uuid", &self.ping_id)
            .field("ping", &self.ping)
            .field("pong", &self.pong)
            .field("batch_size", &self.batch_size)
            .field("buffers", &self.buffers)
            .finish()
    }
}
