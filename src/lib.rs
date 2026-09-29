//! bupr — preset-based mirror backups for macOS.
//! Design: docs/superpowers/specs/2026-09-29-bupr-design.md

pub mod cli;
pub mod config;
pub mod dest;
pub mod engine;
pub mod history;
pub mod plan;
pub mod preflight;
pub mod relpath;
pub mod rules;
pub mod runner;
pub mod scan;
pub mod state;
pub mod ui;
pub mod worker;

/// Destination marker file (spec §3.6).
pub const MARKER_NAME: &str = ".bupr-dest";
/// Prefix of in-progress copies (spec §6.2).
pub const TMP_PREFIX: &str = ".bupr-tmp-";

#[cfg(test)]
mod testutil;

/// Short, menu-sized reason why a preset cannot run.
pub fn menu_reason(e: &preflight::PreflightError) -> String {
    match e {
        preflight::PreflightError::NotMounted(_) => "drive not mounted".to_string(),
        preflight::PreflightError::SourceMissing(_) => "source missing".to_string(),
        other => other.to_string(),
    }
}
