use std::time::Duration;

use crate::packet::NonZeroBatchSize;

// TODO: serde derives

/// Connection related configuration for both server and client.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub struct ConnectionConfig {
    /// How long to wait before trying to send a packet again.
    ///
    /// TODO: consider exponential backoff
    pub resend_delay: Duration,
    /// How often a packet is resent before getting canceled.
    pub max_missed_acks: Option<usize>,
    /// How long to wait for new data before timing out an incoming packet.
    pub receive_timeout: Option<Duration>,
    /// How many packets are sent at once initially for new connections.
    ///
    /// This value is adjusted automatically (on a per-client basis for servers).
    ///
    /// Note, that the total number of packets that a server can send out also scales with the
    /// number of clients that a server is connected to. This value is only per-client.
    pub initial_send_batch_size: NonZeroBatchSize,
    /// A packet that requires more than this number of chunks, will serialize in the background.
    ///
    /// This spawns a background thread and communicates back via a rendezvous channel.
    pub background_serialization_threshold: usize,
}

impl Default for ConnectionConfig {
    fn default() -> Self {
        Self {
            resend_delay: Duration::from_secs(1),
            max_missed_acks: Some(3),
            receive_timeout: Some(Duration::from_secs(10)),
            initial_send_batch_size: NonZeroBatchSize::new(8).unwrap(),
            background_serialization_threshold: 8,
        }
    }
}

/// Server specific configuration.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub struct ServerConfig {
    /// General connection related configuration.
    pub connection: ConnectionConfig,
    /// How long to wait between each ping. Defaults to **2.5 seconds**.
    ///
    /// Clients that fail to respond withing this delay for [`Self::max_missed_pongs`] times will be
    /// timed out.
    pub ping_delay: Duration,
    /// After how many missing pongs a client will be timed out.
    ///
    /// Defaults to **3**, which means the 4th missed pong (after **10 seconds**) causes a timeout.
    ///
    /// If set to [`None`] clients are never timed out. `Some(0)` would time out clients after just
    /// a single missed pong, which is almost guaranteed to happen at some point unless the
    /// connection is incredibly stable.
    pub max_missed_pongs: Option<usize>,
    /// How long to wait before a server forgets a client.
    ///
    /// Defaults to **1 day**, so the list of listeners doesn't grow indefinitely for 24/7 servers.
    ///
    /// Once a listener is forgotten, it will have to query its compatibility again.
    ///
    /// If set to [`None`] the server will never forget any clients unless explicitly told to do so
    /// via [`Server::unlisten`](crate::net::server::Server::unlisten).
    pub listener_timeout: Option<Duration>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            connection: Default::default(),
            ping_delay: Duration::from_millis(2500),
            max_missed_pongs: Some(3),
            listener_timeout: Some(Duration::from_secs(60 * 60 * 24)),
        }
    }
}

/// Client specific configuration.
pub struct ClientConfig {
    /// General connection related configuration.
    pub connection: ConnectionConfig,
    /// How long to wait for a packet before disconnecting.
    ///
    /// Defaults to **10 seconds**.
    ///
    /// Should be considerably greater than [`ServerConfig::ping_delay`] of the server to avoid the
    /// client thinking the server is dead even though it just didn't send any non-ping packets for
    /// a while.
    ///
    /// If set to [`None`], it will never time out and wait indefinitely.
    pub timeout: Option<Duration>,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            connection: Default::default(),
            timeout: Some(Duration::from_secs(10)),
        }
    }
}
