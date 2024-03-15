use std::{
    convert::Infallible,
    env::args,
    net::{Ipv4Addr, SocketAddr},
};

use anyhow::{bail, Result};
use rkyv::{Archive, Serialize};
use ultinet::{
    net::{
        client::{Client, ClientState, Compatibility},
        server::Server,
        BasicLogConnectionHandler,
    },
    packet::{NoPacket, Packets},
};

struct CustomPackets;

impl Packets for CustomPackets {
    type Version = ();
    type CompatibilityError = Infallible;

    type Query = ();
    type Status = ();

    type Connect = ();
    type Disconnect = ();

    type Accept = ();
    type Reject = ();
    type Kick = ();

    type Server = NoPacket;
    type Client = AwesomePacket;

    type ServerUpdate = NoPacket;
    type ClientUpdate = NoPacket;
}

#[derive(Debug, Archive, Serialize)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
struct AwesomePacket {
    name: String,
    age: u8,
    is_awesome: bool,
    big_data: [u8; 4096],
}

fn main() -> Result<()> {
    match args().nth(1).as_deref() {
        Some("server") => server(),
        Some("client") => client(),
        _ => bail!("expected 'server' or 'client'"),
    }
}

fn server() -> Result<()> {
    let mut server = Server::<CustomPackets>::host((Ipv4Addr::LOCALHOST, 42069))?;
    loop {
        server.update(&mut BasicLogConnectionHandler);
        // std::thread::sleep(Duration::from_millis(100));
    }
}

fn client() -> Result<()> {
    let mut client = Client::<CustomPackets>::new()?;

    let server_addr = SocketAddr::from((Ipv4Addr::LOCALHOST, 42069));
    client.listen(server_addr)?;
    while let Some(Compatibility::Pending) = client.compatibility(server_addr) {
        client.update(&mut BasicLogConnectionHandler);
        // std::thread::sleep(Duration::from_millis(1));
    }

    if let Some(compatibility) = client.compatibility(server_addr) {
        match compatibility {
            Compatibility::Incompatible(error) => {
                println!("incompatible: {error}");
                return Ok(());
            }
            Compatibility::Pending => unreachable!(),
            Compatibility::Compatible => {}
        }
    } else {
        println!("error");
        return Ok(());
    }

    client.query(server_addr, ())?;
    client.connect(server_addr, ())?;

    while client.state() != ClientState::Connected {
        client.update(&mut BasicLogConnectionHandler);
        // std::thread::sleep(Duration::from_millis(1));
    }

    client.send(AwesomePacket {
        name: "test ".repeat(200000),
        age: 69,
        is_awesome: true,
        big_data: [42; 4096],
    })?;

    loop {
        client.update(&mut BasicLogConnectionHandler);
        // std::thread::sleep(Duration::from_millis(1));
    }
}
