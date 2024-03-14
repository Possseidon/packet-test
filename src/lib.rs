#![allow(dead_code)]

pub mod net;
pub mod packet;

// temporary for testing

use std::convert::Infallible;

use packet::{NoPacket, NoVersion, Packets};
use rkyv::{Archive, Serialize};

pub struct MyPackets;

#[derive(Debug, Archive, Serialize)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct Big([u8; 3]);

impl Packets for MyPackets {
    type Version = NoVersion;
    type CompatibilityError = Infallible;

    type Query = NoPacket;
    type Status = NoPacket;

    type Connect = NoPacket;
    type Disconnect = NoPacket;

    type Accept = NoPacket;
    type Reject = NoPacket;
    type Kick = NoPacket;

    type Server = NoPacket;
    type Client = NoPacket;

    type ServerUpdate = NoPacket;
    type ClientUpdate = NoPacket;
}

// use std::{
//     io::{self, ErrorKind},
//     iter::once,
//     net::{Ipv4Addr, SocketAddr, UdpSocket},
//     str::Utf8Error,
//     time::{Duration, Instant},
// };

// use bevy::prelude::*;
// use rkyv::{Archive, Deserialize, Serialize};
// use thiserror::Error;
// use uuid::Uuid;

// use packet::{NonZeroBatchSize, ReceivePacketBuffer, SendPacketBuffer, PACKET_BUFFER_SIZE};

// #[derive(Component)]
// pub struct ServerEntity(pub Uuid);

// /// Unreliable packets used for updates.
// trait UpdatePayload {
//     fn payload<'a>(&self, buf: &'a mut [u8]) -> &'a [u8];
// }

// trait ServerUpdate: UpdatePayload {}

// trait ClientUpdate: UpdatePayload {}

// struct Ping(Uuid);

// impl ServerUpdate for Ping {}

// struct Pong(Uuid);

// impl ClientUpdate for Pong {}

// #[derive(Debug, Error)]
// #[error("{0} is already connected")]
// struct AlreadyConnected(SocketAddr);

// #[derive(Debug, Error)]
// #[error("{0} is not connected")]
// struct NotConnected(SocketAddr);

// pub fn handle_client_packets(mut server: ResMut<Server>) {
//     let mut buf = [0; PACKET_BUFFER_SIZE];
//     loop {
//         let (payload, addr) = match server.socket.recv_from(&mut buf) {
//             Ok((size, addr)) => (&buf[..size], addr),
//             Err(error) => match error.kind() {
//                 ErrorKind::WouldBlock => break,
//                 ErrorKind::ConnectionReset => {
//                     // we don't know which client disconnected, so just let it time out
//                     continue;
//                 }
//                 _ => {
//                     error!("{error}");
//                     continue;
//                 }
//             },
//         };
//         match ClientPacket::deserialize(payload) {
//             Ok(packet) => {
//                 server.handle_client_packet(packet, addr);
//             }
//             Err(error) => {
//                 error!("{error}");
//             }
//         }
//     }
// }

// pub fn timeout_clients(mut server: ResMut<Server>) {
//     server.timeout_clients();
// }

// pub fn ping_clients(mut server: ResMut<Server>) {
//     server.ping_clients();
// }

// #[derive(Debug, Error)]
// pub enum PacketDeserializationError {
//     #[error("empty packet")]
//     Empty,
//     #[error("malformed packet")]
//     Malformed,
//     #[error("invalid packet kind: {0}")]
//     InvalidKind(u8),
//     #[error(transparent)]
//     Utf8Error(#[from] Utf8Error),
// }

// #[derive(Archive, Serialize, Deserialize)]
// enum ServerPacket {
//     Accept,
//     Reject,
//     Kick,
// }

// #[derive(Archive, Serialize, Deserialize)]
// enum ClientPacket {
//     Connect,
//     Disconnect,
// }
