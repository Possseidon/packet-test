// use std::time::Instant;

// use bevy::{log::LogPlugin, prelude::*};
// use packet_test::{handle_client_packets, ping_clients, timeout_clients, Server, ServerEntity};
// use uuid::Uuid;

fn main() {
    // App::new()
    //     .add_plugins((MinimalPlugins, LogPlugin::default()))
    //     .add_systems(Startup, (setup, setup_server))
    //     .add_systems(
    //         Update,
    //         (handle_client_packets, timeout_clients, ping_clients)
    //             .run_if(resource_exists::<Server>),
    //     )
    //     .run();
}

// type Version = u64;

// enum ClientState {
//     /// The entity is not loaded.
//     Unloaded,
//     /// The entity is loaded.
//     Loaded {
//         /// When the client acknowledged this version.
//         at: Instant,
//         /// The version that the client has currently loaded.
//         version: Version,
//     },
// }

// enum ClientStateTransition {
//     Load,
//     Unload,
// }

// struct ClientEntity {
//     current: ClientState,
//     transition: Option<ClientStateTransition>,
// }

// /// Contains a list of [`ClientEntity`]s that describes the client.
// struct ServerEntity {
//     clients: Vec<Option<ClientEntity>>,
// }

// fn setup_server(mut commands: Commands) {
//     match Server::new() {
//         Ok(server) => {
//             commands.insert_resource(server);
//         }
//         Err(error) => {
//             error!("{error}");
//         }
//     }
// }

// fn setup(mut commands: Commands) {
//     commands.spawn((SpatialBundle::default(), ServerEntity(Uuid::new_v4())));
// }

// struct PacketPayload {
//     id: u16,
//     size: u16,
//     buf: [u8; 508],
// }

// enum PacketSerializeError {}
// enum PacketDeserializeError {}

// trait Packet: Sized {
//     fn to_payload(&self, payload: &mut PacketPayload) -> Result<(), PacketSerializeError>;
//     fn from_payload(payload: &PacketPayload) -> Result<Self, PacketDeserializeError>;
// }

// /// A packet sent by the server to the client.
// enum ServerPacket {}

// /// A packet sent by the client to the server.
// enum ClientPacket {}

// const MAX_CONNECTIONS: usize = 64;

// struct VersionedState<T> {
//     /// Incremented every time the entity is updated.
//     version: NonZeroU64,
//     /// The version that each client has acknowledged.
//     client_versions: Vec<Option<NonZeroU64>>,
//     /// The current state.
//     state: T,
// }

// impl<T> VersionedState<T> {
//     /// Returns a mutable reference to the state, assuming changes will be made through it.
//     fn get_mut(&mut self) -> &mut T {
//         self.version = self
//             .version
//             .checked_add(1)
//             .expect("overflow should be practically impossible");
//         &mut self.state
//     }

//     /// Returns a read-only reference to the state.
//     fn get(&self) -> &T {
//         &self.state
//     }

//     /// Indices of all clients that do not have the latest version of the state.
//     fn outdated_clients(&self) -> impl Iterator<Item = usize> + '_ {
//         self.client_versions
//             .iter()
//             .enumerate()
//             .filter_map(|(id, version)| (version != &Some(self.version)).then_some(id))
//     }
// }

// struct Server {
//     socket: UdpSocket,
//     connections: Vec<Option<Connection>>,
//     state: ServerState,
// }

// struct ServerState {
//     entities: Vec<VersionedState<EntityData>>,
//     chunks: Vec<VersionedState<Chunk>>,
// }

// struct EntityData {
//     id: Uuid,
//     position: [u8; 3],
// }

// struct Chunk {
//     pos: [u8; 3],
//     blocks: (),
// }

// struct Client {
//     socket: UdpSocket,
//     entities: Vec<EntityData>,
// }

// impl Server {
//     fn send(&self, client: SocketAddr, packet: ServerPacket) {
//         self.send_channel.send(packet).expect("send channel closed");
//     }

//     fn recv(&self) -> ClientPacket {
//         self.recv_channel.recv().expect("recv channel closed")
//     }
// }

// struct Connection {
//     addr: Ipv4Addr,
// }
