//! Best-effort article statistics service.
//!
//! `termblog-statd` is the sole SQLite writer.  Network-facing processes only
//! submit bounded batches over the shared SEQPACKET framing protocol; raw IPs
//! are canonicalized and HMACed before a transaction reaches durable storage.

pub mod client;
pub mod protocol;
pub mod server;
pub mod store;

pub use client::Client;
pub use protocol::*;
