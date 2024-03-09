use std::{
    io::{self, ErrorKind},
    net::{Ipv4Addr, UdpSocket},
    time::{Duration, Instant},
};

use bevy::{prelude::*, sprite::Mesh2dHandle};
use packet_test::{
    Connect, Disconnect, Pong, SerializeFixedSizePacket, ServerEntity, ServerPacket, DEFAULT_PORT, PACKET_BUFFER_SIZE,
};
use uuid::Uuid;

fn main() {
    App::new()
        .add_plugins(DefaultPlugins)
        .add_systems(Startup, (setup, connect_to_server))
        .add_systems(
            Update,
            (handle_server_packets, check_server_alive).run_if(resource_exists::<Client>),
        )
        .run();
}

#[derive(Resource)]
struct Client {
    socket: UdpSocket,
    /// The last time the server sent a ping.
    ping: Instant,
    /// How long the client waits for a ping from the server before it disconnects.
    timeout: Duration,
}

impl Client {
    fn handle_server_packet(&mut self, packet: ServerPacket) {
        match packet {
            ServerPacket::Ping(uuid) => {
                info!("ping from server");
                self.socket.send(&Pong(uuid).serialize());
                self.ping = Instant::now();
            }
            ServerPacket::Accept => {
                info!("Connected!");
            }
            ServerPacket::Reject => {
                warn!("Rejected");
            }
            ServerPacket::Kick => {
                warn!("Kicked");
            }
        }
    }
}

impl Client {
    fn new() -> io::Result<Self> {
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))?;
        socket.connect((Ipv4Addr::LOCALHOST, DEFAULT_PORT))?;
        socket.set_nonblocking(true)?;

        socket.send(&Connect.serialize())?;

        Ok(Self {
            socket,
            ping: Instant::now(),
            timeout: Duration::from_secs(10),
        })
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        // try to notify the server, but it's fine if it fails
        self.socket.send(&Disconnect.serialize()).ok();
    }
}

fn setup(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<ColorMaterial>>,
) {
    commands.spawn(Camera2dBundle::default());

    // TODO: This should be triggered by the server
    commands.spawn((
        ColorMesh2dBundle {
            mesh: Mesh2dHandle(meshes.add(Circle::new(20.0))),
            material: materials.add(Color::rgb(0.5, 0.0, 1.0)),
            ..default()
        },
        ServerEntity(Uuid::nil()),
    ));
}

fn connect_to_server(mut commands: Commands) {
    match Client::new() {
        Ok(client) => {
            commands.insert_resource(client);
        }
        Err(error) => {
            error!("{error}");
        }
    };
}

fn handle_server_packets(mut client: ResMut<Client>, mut commands: Commands) {
    let mut buf = [0; PACKET_BUFFER_SIZE];
    loop {
        let payload = match client.socket.recv(&mut buf) {
            Ok(size) => &buf[..size],
            Err(error) => match error.kind() {
                ErrorKind::WouldBlock => break,
                ErrorKind::ConnectionReset => {
                    warn!("connection reset");
                    commands.remove_resource::<Client>();
                    break;
                }
                _ => {
                    error!("{error}");
                    continue;
                }
            },
        };
        match ServerPacket::deserialize(payload) {
            Ok(packet) => {
                client.handle_server_packet(packet);
            }
            Err(error) => {
                error!("{error}");
            }
        }
    }
}

fn check_server_alive(client: Res<Client>, mut commands: Commands) {
    if client.ping.elapsed() > client.timeout {
        warn!("timed out");
        commands.remove_resource::<Client>();
    }
}
