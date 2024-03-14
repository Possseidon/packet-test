use std::net::Ipv4Addr;

use anyhow::Result;
use packet_test::net::{server::Server, BasicLogConnectionHandler};

fn main() -> Result<()> {
    let mut server = <Server>::host((Ipv4Addr::LOCALHOST, 42069))?;
    loop {
        server.update(&mut BasicLogConnectionHandler);
        // std::thread::sleep(Duration::from_millis(100));
    }
}
