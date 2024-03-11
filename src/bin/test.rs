use std::sync::{Arc, Weak};

use rkyv::{to_bytes, Archive, Deserialize, Serialize};

#[derive(Clone, Debug, Archive, Serialize, Deserialize)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
enum Test {
    Small,
    Big(Option<Weak<[u8; 1024]>>),
}

fn main() {}
