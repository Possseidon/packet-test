use rkyv::{
    check_archived_root,
    ser::{serializers::AllocSerializer, Serializer},
    Archive, Serialize,
};

#[derive(Archive, Serialize)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
struct Person {
    name: String,
    age: i32,
}

// trait Packet: Archive + Serialize<AlignedSerializer<AlignedVec>> {}

fn main() {
    let value = Person {
        name: "test".to_string(),
        age: 10,
    };

    let mut serializer = AllocSerializer::<512>::default();
    serializer.serialize_value(&value).unwrap();
    let mut bytes = serializer.into_serializer().into_inner();
    println!("{:?}", bytes);
    bytes[7] = 0x42;

    let archived = check_archived_root::<Person>(&bytes[..]).unwrap();
    println!("{:?}", archived);
}
