pub mod config;
pub mod net;
pub mod packet;

#[cfg(not(any(feature = "packet_le", feature = "packet_be", feature = "packet_ne")))]
compile_error!("enable either packet_le or packet_be; if you are 100% sure you will never send a packet between computers with different endianness, enable packet_ne");

#[cfg(all(feature = "packet_le", feature = "packet_be"))]
compile_error!("packet_le and packet_be are mutually exclusive");

#[cfg(all(feature = "packet_le", feature = "packet_ne"))]
compile_error!("packet_le and packet_ne are mutually exclusive");

#[cfg(all(feature = "packet_be", feature = "packet_ne"))]
compile_error!("packet_be and packet_ne are mutually exclusive");
