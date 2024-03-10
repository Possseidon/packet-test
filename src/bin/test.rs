use rkyv::{Archive, Deserialize, Serialize};

#[derive(Clone, Debug, Archive, Serialize, Deserialize)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
struct Person {
    name: String,
    age: i32,
}

fn main() {
    let _ = Person {
        name: "testing".repeat(20),
        age: 10,
    };
}
