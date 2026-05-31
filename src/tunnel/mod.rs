//! Userspace egress tunnel.
//!
//! Routes a sandboxed container's internet egress through npxc over `WireGuard`
//! so npxc can filter it. The container runs on a host-only (`--internal`)
//! network with no NAT route to the internet; its only path out is a `WireGuard`
//! tunnel terminated by npxc, which decrypts the traffic and forwards allowed
//! flows to real host sockets.
//!
//! Layers:
//! - [`keys`] — `WireGuard` key material (`Curve25519` keypairs, base64 encoding).

pub mod keys;

pub use keys::WgKeypair;
