//! Plain line output for pipes, unattended runs, dry runs and narrow
//! terminals (spec §6.5, §8.2). No escape codes.

use std::path::Path;
use std::time::{Duration, Instant};

use crate::config::Preset;
use crate::engine::{Event, Mode, Outcome, PlanAction, PlanSummary, RunStats};
use crate::preflight::MarkerStatus;
use crate::ui::dashboard::{DashState, Phase};
use crate::ui::format::{bytes, clock, count, tilde};

pub struct Plain {
    preset: String,
    quiet: bool,
    state: DashState,
    start: Instant,
    last_line: Instant,
}

impl Plain {
    pub fn new(preset: &str, mode: Mode, quiet: bool) -> Plain {
        Plain {
            preset: preset.to_string(),
            quiet,
            state: DashState::new(preset, mode),
            start: Instant::now(),
            last_line: Instant::now(),
        }
    }

    pub fn event(&mut self, ev: &Event) {
        self.state.apply(ev);
        if self.quiet {
            return;
        }
        match ev {
            Event::Warning { message } => println!("  ! {message}"),
            Event::PlanItem { action, path } => println!("  {} {path}", action_symbol(*action)),
            _ => {}
        }
        let busy = matches!(self.state.phase, Phase::Copying | Phase::Deleting)
            && self.state.total_files > 0;
        if busy && self.last_line.elapsed() >= Duration::from_secs(5) {
            self.state.tick(self.start.elapsed().as_secs_f64());
            println!("{}", progress_line(&self.preset, &self.state));
            self.last_line = Instant::now();
        }
    }
}

pub fn action_symbol(a: PlanAction) -> &'static str {
    match a {
        PlanAction::Copy => "+",
        PlanAction::Link => "~",
        PlanAction::Mkdir => "d",
        PlanAction::Rename => ">",
        PlanAction::Delete => "-",
    }
}

pub fn progress_line(preset: &str, s: &DashState) -> String {
    let eta = s
        .eta_secs()
        .map_or_else(String::new, |e| format!(" · eta {}", clock(e)));
    format!(
        "  {preset}: {:>3}% · {} / {} · {}/{} files · {}/s{eta}",
        (s.ratio() * 100.0).floor() as u64,
        bytes(s.done_bytes),
        bytes(s.total_bytes),
        count(s.done_files),
        count(s.total_files),
        bytes(s.speed as u64)
    )
}

pub fn planned_line(s: &PlanSummary) -> String {
    let t = &s.totals;
    format!(
        "  {} files ({}) to copy · {} to delete · {} unchanged",
        count(t.copy_files),
        bytes(t.copy_bytes),
        count(t.delete_entries),
        count(t.unchanged_files)
    )
}

pub fn plan_lines(preset: &Preset, s: &PlanSummary, home: &Path) -> Vec<String> {
    let t = &s.totals;
    let mut v = vec![
        format!(
            "  copy       {} files ({})",
            count(t.copy_files),
            bytes(t.copy_bytes)
        ),
        format!(
            "  unchanged  {} files ({})",
            count(t.unchanged_files),
            bytes(t.unchanged_bytes)
        ),
    ];
    let mut del = format!(
        "  delete     {} entries ({})",
        count(t.delete_entries),
        bytes(t.delete_bytes)
    );
    if s.over_delete_limit {
        del += &format!(
            " — over the limit ({} / {}); would ask",
            count(preset.max_delete),
            bytes(preset.max_delete_bytes)
        );
    }
    v.push(del);
    let folder = match &s.marker {
        MarkerStatus::Fresh => "new backup folder".to_string(),
        MarkerStatus::Matches => "bupr folder for this preset".to_string(),
        MarkerStatus::Foreign => {
            "contains files bupr did not put there — would ask first".to_string()
        }
        MarkerStatus::Mismatch { preset: p, source } => {
            format!(
                "belongs to preset \"{p}\" ({}) — would ask first",
                tilde(source, home)
            )
        }
    };
    v.push(format!("  folder     {folder}"));
    if let Some(f) = s.free_bytes {
        let warn = if f < s.needed_bytes {
            " — NOT ENOUGH"
        } else {
            ""
        };
        v.push(format!(
            "  space      {} free, {} needed{warn}",
            bytes(f),
            bytes(s.needed_bytes)
        ));
    }
    if s.secret_files > 0 {
        v.push(format!(
            "  secrets    {} file(s) such as .env or keys",
            count(s.secret_files)
        ));
    }
    let other_disks = s.skipped_mounts + s.dest_mounts;
    if s.skipped_special + other_disks + s.collisions + s.scan_errors > 0 {
        v.push(format!(
            "  skipped    {} special, {} on other disks, {} name collisions, {} unreadable",
            s.skipped_special, other_disks, s.collisions, s.scan_errors
        ));
    }
    v
}

pub fn summary_line(preset: &str, stats: &RunStats, elapsed: u64) -> String {
    let t = clock(elapsed);
    let tag = match stats.mode {
        Mode::Run => "",
        Mode::DryRun => " (dry run)",
        Mode::Simulate => " (simulated)",
    };
    let (copied, deleted) = if stats.mode == Mode::Simulate {
        ("read", "would be deleted")
    } else {
        ("copied", "deleted")
    };
    let work = format!(
        "{} files ({}) {copied}, {} {deleted}, {} unchanged",
        count(stats.copied_files),
        bytes(stats.copied_bytes),
        count(stats.deleted),
        count(stats.unchanged)
    );
    let n = stats.errors.len();
    let errs = format!("{n} error{}", if n == 1 { "" } else { "s" });
    let msg = stats.message.as_deref();
    match (stats.outcome, stats.mode) {
        (Outcome::Ok, Mode::DryRun) => format!("✓ {preset}{tag} · nothing was changed · {t}"),
        (Outcome::Errors, Mode::DryRun) => {
            format!("! {preset}{tag} · {errs} while scanning; nothing was changed · {t}")
        }
        (Outcome::Ok, _) => format!("✓ {preset}{tag} · {work} · {t}"),
        (Outcome::Errors, _) => format!("! {preset}{tag} · {work} · {errs} · {t}"),
        (Outcome::DeletionsSkipped, _) => {
            format!("! {preset}{tag} · {work} · deletions skipped · {t}")
        }
        (Outcome::Aborted, _) => match msg {
            Some(m) => format!("✗ {preset}{tag} · aborted: {m}"),
            None => format!("✗ {preset}{tag} · aborted"),
        },
        (Outcome::Interrupted, _) => format!(
            "✗ {preset}{tag} · interrupted after {} files ({}) · {t}",
            count(stats.copied_files),
            bytes(stats.copied_bytes)
        ),
        (Outcome::PreflightFailed, _) => {
            format!("✗ {preset}{tag} · {}", msg.unwrap_or("preflight failed"))
        }
    }
}

pub fn error_lines(stats: &RunStats, max: usize) -> Vec<String> {
    let mut v: Vec<String> = stats
        .errors
        .iter()
        .take(max)
        .map(|e| format!("    ✗ {}: {}", e.path, e.message))
        .collect();
    if stats.errors.len() > max {
        v.push(format!(
            "    … and {} more (see `bupr log`)",
            stats.errors.len() - max
        ));
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{FileErr, Outcome};

    fn stats(outcome: Outcome, mode: Mode) -> RunStats {
        let mut s = RunStats::new(mode);
        s.outcome = outcome;
        s.copied_files = 8214;
        s.copied_bytes = 5_400_000_000;
        s.deleted = 12;
        s.unchanged = 96_110;
        s
    }

    #[test]
    fn summary_lines() {
        assert_eq!(
            summary_line("dev", &stats(Outcome::Ok, Mode::Run), 38),
            "✓ dev · 8,214 files (5.4 GB) copied, 12 deleted, 96,110 unchanged · 0:38"
        );
        assert_eq!(
            summary_line("dev", &stats(Outcome::Ok, Mode::Simulate), 38),
            "✓ dev (simulated) · 8,214 files (5.4 GB) read, 12 would be deleted, 96,110 unchanged · 0:38"
        );
        assert_eq!(
            summary_line("dev", &stats(Outcome::Ok, Mode::DryRun), 1),
            "✓ dev (dry run) · nothing was changed · 0:01"
        );
        let mut e = stats(Outcome::Errors, Mode::Run);
        e.errors = vec![FileErr {
            path: "a".into(),
            message: "denied".into(),
        }];
        assert!(summary_line("dev", &e, 38).starts_with("! dev · 8,214 files"));
        assert!(summary_line("dev", &e, 38).contains("1 error"));
        let mut f = stats(Outcome::PreflightFailed, Mode::Run);
        f.message = Some("drive not mounted: /Volumes/Backup is not available".into());
        assert_eq!(
            summary_line("dev", &f, 0),
            "✗ dev · drive not mounted: /Volumes/Backup is not available"
        );
        assert!(
            summary_line("dev", &stats(Outcome::DeletionsSkipped, Mode::Run), 5)
                .contains("deletions skipped")
        );
        assert!(
            summary_line("dev", &stats(Outcome::Interrupted, Mode::Run), 5).contains("interrupted")
        );
    }

    #[test]
    fn error_lines_are_capped() {
        let mut s = stats(Outcome::Errors, Mode::Run);
        s.errors = (0..12)
            .map(|i| FileErr {
                path: format!("f{i}"),
                message: "x".into(),
            })
            .collect();
        let lines = error_lines(&s, 10);
        assert_eq!(lines.len(), 11);
        assert_eq!(lines[0], "    ✗ f0: x");
        assert_eq!(lines[10], "    … and 2 more (see `bupr log`)");
    }
}
