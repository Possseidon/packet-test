use std::{net::UdpSocket, sync::Weak, time::Duration};

use rkyv::{Archive, Deserialize, Serialize};

#[derive(Clone, Debug, Archive, Serialize, Deserialize)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
enum Test {
    Small,
    Big(Option<Weak<[u8; 1024]>>),
}

fn main() {
    let server = UdpSocket::bind("127.0.0.1:42069").unwrap();
    server.set_nonblocking(true).unwrap();

    let client = UdpSocket::bind("127.0.0.1:0").unwrap();
    client.set_nonblocking(true).unwrap();
    client.set_broadcast(true).unwrap();
    client.send_to(&[1, 2, 3], ("<broadcast>", 42069)).unwrap();

    loop {
        let mut buf = [0; 3];
        let result = server.recv_from(&mut buf);
        println!("{result:?}");
        std::thread::sleep(Duration::from_millis(500));
    }
}
