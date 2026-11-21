//! The shared server update engine for snds service apps (docs/self-hosting-service-apps.md).
//!
//! Every self-hosted product ships one service app of its own, which runs the product's helper
//! tooling and its updates. Each product updates itself, never another product. This crate is the
//! part they share:
//!
//! - [`release`]: the signed channel (a Tauri-style `latest.json`, one `server-<service>` entry per
//!   image, each signed over its `sy-server-release/1` statement, plus a signed by-hand flag).
//! - [`docker`]: the containers, through the Docker Engine API behind the service app's own socket
//!   proxy. Only containers labelled for the product; images only by digest.
//! - [`engine`]: download ahead of time, swap, health check, rollback, the person's automatic or
//!   by-hand choice, and the product's own steps around an update.
//! - [`shell`] (feature `shell`): the service app itself: its config, settings file, HTTP API and
//!   timer. The `service-app` binary runs it.
//!
//! Ported from Synapse's relay helper (`relay/src/updates/`, Stage L L5.12; security review LS18;
//! exit test X15e), with Synapse #233's registry sign-in.

pub mod docker;
pub mod engine;
pub mod release;
#[cfg(feature = "shell")]
pub mod shell;

pub use engine::{
    Feed, Hooks, Mode, NoHooks, Policy, Record, Saved, Settings, State, Status, Store, Trigger,
    Updater,
};
