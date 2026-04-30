//! Operation handlers for `POST /graphql/v2`. Each module covers a small group
//! of operations sharing types or backend behavior.
//!
//! - [`canned`] — identity / settings / workspaces / model-lists / experiments,
//!   returning canned local-mode responses so the client never reaches
//!   `app.warp.dev`.
//! - [`ai`] — `generateCommands` and `generateDialogue`, which call the
//!   configured AI backend.
//! - [`cloud_stubs`] — empty-`Ok` responses for cloud-only polling APIs that
//!   the client retries on error (see rubber-duck findings in plan.md).

pub mod ai;
pub mod canned;
pub mod cloud_stubs;
