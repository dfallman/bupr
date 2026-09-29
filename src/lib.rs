//! bupr — preset-based mirror backups for macOS.
//! Design: docs/superpowers/specs/2026-09-29-bupr-design.md

pub mod config;
pub mod relpath;
pub mod rules;

#[cfg(test)]
mod testutil;
