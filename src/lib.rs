//! bupr — preset-based mirror backups for macOS.
//! Design: docs/superpowers/specs/2026-09-29-bupr-design.md

pub mod config;
pub mod dest;
pub mod engine;
pub mod plan;
pub mod preflight;
pub mod relpath;
pub mod rules;
pub mod scan;

/// Destination marker file (spec §3.6).
pub const MARKER_NAME: &str = ".bupr-dest";
/// Prefix of in-progress copies (spec §6.2).
pub const TMP_PREFIX: &str = ".bupr-tmp-";

#[cfg(test)]
mod testutil;
