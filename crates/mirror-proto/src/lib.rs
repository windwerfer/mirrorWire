pub mod clock;
pub mod control;
pub mod fragment;
pub mod stats;

pub const PROTOCOL_VERSION: u32 = 1;
pub const ALPN: &[u8] = b"mirror/1";
pub const MDNS_SERVICE: &str = "_mirror._udp.local.";
pub const DEFAULT_PORT: u16 = 47_800;
