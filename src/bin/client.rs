// use std::{
//     io::{self, ErrorKind},
//     net::{Ipv4Addr, UdpSocket},
//     time::{Duration, Instant},
// };

// use bevy::{prelude::*, sprite::Mesh2dHandle};
// use packet_test::{
//     Connect, Disconnect, Pong, SerializeFixedSizePacket, ServerEntity, ServerPacket, DEFAULT_PORT,
//     PACKET_BUFFER_SIZE,
// };
// use uuid::Uuid;

use std::{
    net::{Ipv4Addr, SocketAddr},
    time::Duration,
};

use anyhow::Result;
use packet_test::net::{
    client::{Client, Compatibility},
    DefaultConnectionHandler,
};

fn main() -> Result<()> {
    let mut client = <Client>::new()?;

    let server_addr = SocketAddr::from((Ipv4Addr::LOCALHOST, 42069));
    client.listen(server_addr)?;
    while let Some(Compatibility::Pending) = client.compatibility(server_addr) {
        client.update(&mut DefaultConnectionHandler);
        std::thread::sleep(Duration::from_millis(100));
    }

    if let Some(Compatibility::Incompatible(error)) = client.compatibility(server_addr) {
        println!("incompatible: {error}");
        return Ok(());
    }

    println!("connecting...");
    client.connect(server_addr, ())?;

    loop {
        client.update(&mut DefaultConnectionHandler);
        std::thread::sleep(Duration::from_millis(100));
    }
}

// fn main() {
//     App::new()
//         .add_plugins(DefaultPlugins)
//         .add_systems(Startup, (setup, connect_to_server))
//         .add_systems(
//             Update,
//             (handle_server_packets, check_server_alive).run_if(resource_exists::<Client>),
//         )
//         .run();
// }

// fn setup(
//     mut commands: Commands,
//     mut meshes: ResMut<Assets<Mesh>>,
//     mut materials: ResMut<Assets<ColorMaterial>>,
// ) {
//     commands.spawn(Camera2dBundle::default());

//     // TODO: This should be triggered by the server
//     commands.spawn((
//         ColorMesh2dBundle {
//             mesh: Mesh2dHandle(meshes.add(Circle::new(20.0))),
//             material: materials.add(Color::rgb(0.5, 0.0, 1.0)),
//             ..default()
//         },
//         ServerEntity(Uuid::nil()),
//     ));
// }

// fn connect_to_server(mut commands: Commands) {
//     match Client::new() {
//         Ok(client) => {
//             commands.insert_resource(client);
//         }
//         Err(error) => {
//             error!("{error}");
//         }
//     };
// }

// fn handle_server_packets(mut client: ResMut<Client>, mut commands: Commands) {
//     let mut buf = [0; PACKET_BUFFER_SIZE];
//     loop {
//         let payload = match client.socket.recv(&mut buf) {
//             Ok(size) => &buf[..size],
//             Err(error) => match error.kind() {
//                 ErrorKind::WouldBlock => break,
//                 ErrorKind::ConnectionReset => {
//                     warn!("connection reset");
//                     commands.remove_resource::<Client>();
//                     break;
//                 }
//                 _ => {
//                     error!("{error}");
//                     continue;
//                 }
//             },
//         };
//         match ServerPacket::deserialize(payload) {
//             Ok(packet) => {
//                 client.handle_server_packet(packet);
//             }
//             Err(error) => {
//                 error!("{error}");
//             }
//         }
//     }
// }

// fn check_server_alive(client: Res<Client>, mut commands: Commands) {
//     if client.ping.elapsed() > client.timeout {
//         warn!("timed out");
//         commands.remove_resource::<Client>();
//     }
// }
