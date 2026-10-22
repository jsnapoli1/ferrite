//! Model Context Protocol for Ferrite: a dual-era server (2026-07-28 per-request metadata
//! plus the legacy `initialize` handshake) and a matching client.
//!
//! MCP is how Ferrite opens itself to AI: every capability of the server — files, Talk,
//! boards, live documents, search, the Craft apps — is a tool here, and the built-in
//! assistant uses exactly the same tools as any external MCP client would.

pub mod client;
pub mod protocol;
pub mod server;

pub use client::{Client, ClientError, Era};
pub use protocol::*;
pub use server::{Handler, Reply, Server};
