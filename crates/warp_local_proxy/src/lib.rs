//! `warp_local_proxy` — a fork-owned HTTP server that the Warp client points at
//! (via `WARP_SERVER_ROOT_URL`) instead of `https://app.warp.dev`.
//!
//! ## Goals
//!
//! - Handle a small set of GraphQL operations and one REST endpoint that carry
//!   AI inference, by calling a user-configured OpenAI-compatible (or Azure
//!   OpenAI / Azure AI Foundry) backend.
//! - Stub the identity, workspace, settings, and experiments operations the
//!   client makes on launch with canned local-mode responses, so the client
//!   never reaches `app.warp.dev` and never tries to log in.
//! - Reject everything else with a structured GraphQL `errors` payload so the
//!   client surfaces "feature unavailable in local mode" rather than crashing.
//!
//! See the crate-level `README.md` for end-to-end setup instructions.

pub mod config;
pub mod handlers;
pub mod operations;
pub mod server;
pub mod upstream;
