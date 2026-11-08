//! The restart helper for a Docker host (the unified updates proposal, U5).
//!
//! A server can't replace its own image while it runs, and must never hold the Docker socket.
//! This helper runs beside it on the host, holds the socket, and does three things for a
//! server that signs its request with its own key:
//!
//! - **status:** the running image (tag, digest, version) of each allowed service;
//! - **update:** pull the allowed image at an allowed tag, check its digest, and recreate the
//!   allowed services on it (`docker compose up -d`);
//! - **restart:** restart the allowed services (Reload).
//!
//! What a stack may touch is fixed by the host's allow-list ([`allow`]); nothing in a request
//! can name a command, a path, an image or a service outside it. The wire format is in
//! [`protocol`], and [`Client`] is what a server calls the helper with.
//!
//! With `default-features = false`, only the protocol and the client are built: that is what
//! the servers depend on.

#[cfg(feature = "server")]
pub mod allow;
pub mod client;
#[cfg(feature = "server")]
pub mod docker;
mod http;
pub mod protocol;
#[cfg(feature = "server")]
pub mod server;

pub use client::{Client, ClientError, Endpoint};
pub use protocol::{
    ErrorReply, Key, RestartReply, ServiceStatus, StatusReply, UpdateReply, UpdateRequest,
};
