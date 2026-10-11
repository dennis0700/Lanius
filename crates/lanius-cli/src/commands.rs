//! Implementations of `lanius-cli`'s subcommands.
//!
//! Each subcommand lives in its own submodule (mirroring the pattern
//! already established by `update`) and exposes a single `run` entry
//! point, re-exported here under the subcommand's name so `main` can call
//! e.g. [`replay`] / [`probe`] / [`usage`] / [`models`] / [`update`]
//! without reaching into the submodules directly. `events` is shared,
//! private plumbing used by both `replay` and `probe` to drain and
//! summarize a decoded [`lanius_core::upstream::KiroEvent`] stream.

mod events;
mod mcp;
mod models;
mod probe;
mod replay;
mod update;
mod usage;

pub(crate) use mcp::run as mcp;
pub(crate) use models::run as models;
pub(crate) use probe::run as probe;
pub(crate) use replay::run as replay;
pub(crate) use update::run as update;
pub(crate) use usage::run as usage;
