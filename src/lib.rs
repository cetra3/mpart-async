/// Client structs for use when sending requests
pub mod client;
/// Server structs for use when parsing requests
pub mod server;

/// The FileStream is a tokio way of streaming out a file from a given path.
#[cfg(feature = "filestream")]
pub mod filestream;
