//! Test infrastructure: an HTTPS gateway with fault injection, a deterministic mock server and a real
//! impaired network built from Linux namespaces.

pub mod cert;
pub mod gateway;
pub mod mock;
pub mod netlab;
