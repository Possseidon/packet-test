use std::{
    net::UdpSocket,
    time::{Duration, Instant},
};

use crate::packet::{PacketBuffers, Packets};

// TODO: Make ServerPacket and ClientPacket generic
pub struct Client<P: Packets> {
    socket: UdpSocket,
    /// The last time the server sent a ping.
    ping: Instant,
    /// How long the client waits for a ping from the server before it disconnects.
    timeout: Duration,
    buffers: PacketBuffers<P::Client, P::Server>,
}

// impl Client {
//     fn new() -> io::Result<Self> {
//         let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))?;
//         socket.connect((Ipv4Addr::LOCALHOST, Server::DEFAULT_PORT))?;
//         socket.set_nonblocking(true)?;

//         socket.send(&Connect.serialize())?;

//         Ok(Self {
//             socket,
//             ping: Instant::now(),
//             timeout: Duration::from_secs(10),
//         })
//     }

//     fn handle_server_packet(&mut self, packet: ServerPacket) {
//         match packet {
//             ServerPacket::Ping(uuid) => {
//                 info!("ping from server");
//                 self.socket.send(&Pong(uuid).serialize());
//                 self.ping = Instant::now();
//             }
//             ServerPacket::Accept => {
//                 info!("Connected!");
//             }
//             ServerPacket::Reject => {
//                 warn!("Rejected");
//             }
//             ServerPacket::Kick => {
//                 warn!("Kicked");
//             }
//         }
//     }
// }

// impl Drop for Client {
//     fn drop(&mut self) {
//         // try to notify the server, but it's fine if it fails
//         self.socket.send(&Disconnect.serialize()).ok();
//     }
// }
