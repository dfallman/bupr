//! Run history records (spec §8.6). Reading only; writing is in `state`.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::engine::{FileErr, Mode, Outcome, RunStats};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRecord {
    pub preset: String,
    pub started: String,
    pub ended: String,
    pub duration_ms: u64,
    pub outcome: Outcome,
    pub mode: Mode,
    pub unattended: bool,
    pub copied_files: u64,
    pub copied_bytes: u64,
    pub deleted: u64,
    pub unchanged: u64,
    pub warnings: u64,
    pub errors: Vec<FileErr>,
    #[serde(default)]
    pub message: Option<String>,
}

impl RunRecord {
    pub fn new(
        preset: &str,
        started: jiff::Timestamp,
        ended: jiff::Timestamp,
        unattended: bool,
        stats: &RunStats,
    ) -> RunRecord {
        RunRecord {
            preset: preset.to_string(),
            started: started.to_string(),
            ended: ended.to_string(),
            duration_ms: (ended.as_millisecond() - started.as_millisecond()).max(0) as u64,
            outcome: stats.outcome,
            mode: stats.mode,
            unattended,
            copied_files: stats.copied_files,
            copied_bytes: stats.copied_bytes,
            deleted: stats.deleted,
            unchanged: stats.unchanged,
            warnings: stats.warnings,
            errors: stats.errors.iter().take(100).cloned().collect(),
            message: stats.message.clone(),
        }
    }

    pub fn started_at(&self) -> Option<jiff::Timestamp> {
        self.started.parse().ok()
    }
}

pub fn read_all(path: &Path) -> Vec<RunRecord> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// The latest real backup of `preset` that actually ran (not a dry run,
/// simulation, abort or preflight failure).
pub fn last_real_run<'a>(records: &'a [RunRecord], preset: &str) -> Option<&'a RunRecord> {
    records.iter().rev().find(|r| {
        r.preset == preset
            && r.mode == Mode::Run
            && matches!(
                r.outcome,
                Outcome::Ok | Outcome::Errors | Outcome::DeletionsSkipped
            )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Mode, Outcome, RunStats};

    fn rec(preset: &str, mode: Mode, outcome: Outcome, started: &str) -> RunRecord {
        let mut stats = RunStats::new(mode);
        stats.outcome = outcome;
        let t: jiff::Timestamp = started.parse().unwrap();
        RunRecord::new(preset, t, t, false, &stats)
    }

    #[test]
    fn last_real_run_ignores_dry_simulated_and_failed_runs() {
        let v = vec![
            rec("dev", Mode::Run, Outcome::Ok, "2026-09-01T10:00:00Z"),
            rec("dev", Mode::DryRun, Outcome::Ok, "2026-09-02T10:00:00Z"),
            rec("dev", Mode::Simulate, Outcome::Ok, "2026-09-03T10:00:00Z"),
            rec(
                "dev",
                Mode::Run,
                Outcome::PreflightFailed,
                "2026-09-04T10:00:00Z",
            ),
            rec("music", Mode::Run, Outcome::Ok, "2026-09-05T10:00:00Z"),
        ];
        assert_eq!(
            last_real_run(&v, "dev").unwrap().started,
            "2026-09-01T10:00:00Z"
        );
        assert!(last_real_run(&v, "media").is_none());
    }

    #[test]
    fn errors_are_capped_at_100() {
        let mut stats = RunStats::new(Mode::Run);
        stats.errors = (0..150)
            .map(|i| FileErr {
                path: i.to_string(),
                message: "x".into(),
            })
            .collect();
        let t = jiff::Timestamp::now();
        assert_eq!(RunRecord::new("dev", t, t, true, &stats).errors.len(), 100);
    }
}
