//! Scan → plan → decide → execute (spec §6). Runs inside the worker process,
//! or in-process in tests. Emits `Event`s and asks for one `Decision`.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::MARKER_NAME;
use crate::config::Preset;
use crate::dest::{Dest, DestOps, Marker, SimulatedDest};
use crate::plan::{self, DeleteItem, Plan, Totals};
use crate::preflight::{self, Env, MarkerStatus};
use crate::relpath::RelPath;
use crate::scan::{self, Kind};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    Scanning {
        files: u64,
        bytes: u64,
    },
    Planned {
        summary: PlanSummary,
    },
    PlanItem {
        action: PlanAction,
        path: String,
    },
    FileStart {
        path: String,
        size: u64,
    },
    /// Bytes processed since the previous `FileProgress`.
    FileProgress {
        bytes: u64,
    },
    FileDone {
        path: String,
    },
    FileError {
        path: String,
        message: String,
    },
    Deleting {
        total: u64,
    },
    Deleted {
        path: String,
    },
    Warning {
        message: String,
    },
    Fatal {
        message: String,
    },
    Done {
        stats: RunStats,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanAction {
    Rename,
    Mkdir,
    Copy,
    Link,
    Delete,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlanSummary {
    pub source: PathBuf,
    pub dest: PathBuf,
    pub totals: Totals,
    pub marker: MarkerStatus,
    pub free_bytes: Option<u64>,
    pub needed_bytes: u64,
    pub over_delete_limit: bool,
    pub insufficient_space: bool,
    pub secret_files: u64,
    pub largest_deletes: Vec<(String, u64)>,
    pub skipped_special: u64,
    pub skipped_mounts: u64,
    pub collisions: u64,
    pub scan_errors: u64,
    pub case_insensitive: bool,
}

impl PlanSummary {
    pub fn needs_prompt(&self) -> bool {
        self.marker.needs_adoption() || self.insufficient_space || self.over_delete_limit
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum Decision {
    Proceed { allow_deletes: bool, adopt: bool },
    Abort,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Run,
    DryRun,
    Simulate,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunOptions {
    pub mode: Mode,
    /// Emit a `PlanItem` for every planned action.
    pub list_plan: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Ok,
    Errors,
    DeletionsSkipped,
    Aborted,
    Interrupted,
    PreflightFailed,
}

impl Outcome {
    pub fn exit_code(self) -> i32 {
        match self {
            Outcome::Ok => 0,
            Outcome::Errors | Outcome::DeletionsSkipped => 1,
            Outcome::Aborted | Outcome::Interrupted | Outcome::PreflightFailed => 2,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileErr {
    pub path: String,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunStats {
    pub outcome: Outcome,
    pub mode: Mode,
    pub copied_files: u64,
    pub copied_bytes: u64,
    pub deleted: u64,
    pub unchanged: u64,
    pub warnings: u64,
    pub errors: Vec<FileErr>,
    /// Why the run stopped early, if it did.
    pub message: Option<String>,
}

impl RunStats {
    pub fn new(mode: Mode) -> RunStats {
        RunStats {
            outcome: Outcome::Ok,
            mode,
            copied_files: 0,
            copied_bytes: 0,
            deleted: 0,
            unchanged: 0,
            warnings: 0,
            errors: Vec::new(),
            message: None,
        }
    }
}

struct Stop {
    outcome: Outcome,
    message: Option<String>,
}

fn stop(outcome: Outcome, message: impl Into<String>) -> Stop {
    Stop {
        outcome,
        message: Some(message.into()),
    }
}

fn error(stats: &mut RunStats, emit: &mut dyn FnMut(Event), path: &str, message: impl ToString) {
    let message = message.to_string();
    emit(Event::FileError {
        path: path.to_string(),
        message: message.clone(),
    });
    stats.errors.push(FileErr {
        path: path.to_string(),
        message,
    });
}

fn warn(stats: &mut RunStats, emit: &mut dyn FnMut(Event), message: String) {
    stats.warnings += 1;
    emit(Event::Warning { message });
}

pub fn run(
    preset: &Preset,
    env: &Env,
    opts: &RunOptions,
    emit: &mut dyn FnMut(Event),
    decide: &mut dyn FnMut(&PlanSummary) -> Decision,
    cancel: &AtomicBool,
) -> RunStats {
    let mut stats = RunStats::new(opts.mode);
    if let Err(s) = execute(preset, env, opts, emit, decide, cancel, &mut stats) {
        stats.outcome = s.outcome;
        if let Some(m) = &s.message {
            emit(Event::Fatal { message: m.clone() });
        }
        stats.message = s.message;
    }
    emit(Event::Done {
        stats: stats.clone(),
    });
    stats
}

fn list_plan(plan: &Plan, emit: &mut dyn FnMut(Event)) {
    let mut item = |action, path: String| emit(Event::PlanItem { action, path });
    for (from, to) in &plan.renames {
        item(PlanAction::Rename, format!("{from} → {to}"));
    }
    for m in &plan.mkdirs {
        item(PlanAction::Mkdir, m.to_string());
    }
    for c in &plan.copies {
        item(PlanAction::Copy, c.rel.to_string());
    }
    for l in &plan.links {
        item(PlanAction::Link, l.rel.to_string());
    }
    for d in plan.replace_trees.iter().chain(&plan.deletes) {
        item(PlanAction::Delete, d.rel.to_string());
    }
}

fn remove(dest: &dyn DestOps, d: &DeleteItem) -> std::io::Result<()> {
    if d.kind == Kind::Dir {
        dest.remove_dir(&d.rel)
    } else {
        dest.remove_nondir(&d.rel)
    }
}

fn execute(
    preset: &Preset,
    env: &Env,
    opts: &RunOptions,
    emit: &mut dyn FnMut(Event),
    decide: &mut dyn FnMut(&PlanSummary) -> Decision,
    cancel: &AtomicBool,
    stats: &mut RunStats,
) -> Result<(), Stop> {
    let interrupted = || Stop {
        outcome: Outcome::Interrupted,
        message: None,
    };

    // Preflight again inside the worker: defence in depth (spec §3.5).
    let resolved = preflight::check_paths(preset, env)
        .map_err(|e| stop(Outcome::PreflightFailed, e.to_string()))?;
    let filter = preset
        .filter()
        .map_err(|e| stop(Outcome::PreflightFailed, e))?;

    let mut last = Instant::now();
    let src = scan::scan_source(
        &resolved.source,
        &filter,
        &mut |files, bytes| {
            if last.elapsed() >= Duration::from_millis(100) {
                emit(Event::Scanning { files, bytes });
                last = Instant::now();
            }
        },
        cancel,
    )
    .map_err(|e| {
        if cancel.load(Ordering::Relaxed) {
            interrupted()
        } else {
            stop(Outcome::PreflightFailed, format!("cannot scan source: {e}"))
        }
    })?;
    let dst = scan::scan_dest(&resolved.dest, cancel).map_err(|e| {
        if cancel.load(Ordering::Relaxed) {
            interrupted()
        } else {
            stop(
                Outcome::PreflightFailed,
                format!("cannot scan destination: {e}"),
            )
        }
    })?;

    // Unreadable source paths are errors: their destination copies would
    // otherwise look extraneous and be deleted.
    for (p, m) in &src.errors {
        error(stats, emit, p, m);
    }
    for (p, m) in &dst.errors {
        warn(stats, emit, format!("destination {p}: {m}"));
    }

    let ci = preflight::is_case_insensitive(&resolved.dest);
    let plan = plan::build(&src.entries, &dst.entries, &dst.temp_files, ci);
    for (skipped, kept) in &plan.collisions {
        error(
            stats,
            emit,
            skipped.as_str(),
            format!("name collides with {kept} on a case-insensitive destination; skipped"),
        );
    }
    let marker = preflight::marker_status(&resolved.dest, &preset.name, &resolved.source);
    let free = preflight::free_space(&resolved.dest).ok();
    let needed = plan
        .totals
        .copy_bytes
        .saturating_sub(plan.totals.replaced_bytes);
    let mut largest: Vec<(String, u64)> = plan
        .deletes
        .iter()
        .chain(&plan.replace_trees)
        .filter(|d| d.kind == Kind::File)
        .map(|d| (d.rel.to_string(), d.size))
        .collect();
    largest.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    largest.truncate(5);
    let summary = PlanSummary {
        source: resolved.source.clone(),
        dest: resolved.dest.clone(),
        totals: plan.totals.clone(),
        marker: marker.clone(),
        free_bytes: free,
        needed_bytes: needed,
        over_delete_limit: plan.totals.delete_entries > preset.max_delete
            || plan.totals.delete_bytes > preset.max_delete_bytes,
        insufficient_space: opts.mode == Mode::Run && free.is_some_and(|f| f < needed),
        secret_files: src.secret_files,
        largest_deletes: largest,
        skipped_special: src.skipped_special,
        skipped_mounts: src.skipped_mounts,
        collisions: plan.collisions.len() as u64,
        scan_errors: (src.errors.len() + dst.errors.len()) as u64,
        case_insensitive: ci,
    };
    emit(Event::Planned {
        summary: summary.clone(),
    });
    if opts.list_plan {
        list_plan(&plan, emit);
    }
    stats.unchanged = plan.totals.unchanged_files;
    if opts.mode == Mode::DryRun {
        stats.outcome = if stats.errors.is_empty() {
            Outcome::Ok
        } else {
            Outcome::Errors
        };
        return Ok(());
    }

    let (allow_deletes, adopt) = match decide(&summary) {
        Decision::Abort => {
            return Err(Stop {
                outcome: Outcome::Aborted,
                message: None,
            });
        }
        Decision::Proceed {
            allow_deletes,
            adopt,
        } => (allow_deletes, adopt),
    };
    if marker.needs_adoption() && !adopt {
        return Err(stop(
            Outcome::Aborted,
            format!(
                "{} is not a bupr folder for this preset (no matching {MARKER_NAME}); refusing to touch it",
                resolved.dest.display()
            ),
        ));
    }

    let real;
    let dest: &dyn DestOps = if opts.mode == Mode::Simulate {
        &SimulatedDest
    } else {
        real = Dest::open(&resolved.dest).map_err(|e| {
            stop(
                Outcome::PreflightFailed,
                format!("cannot open {}: {e}", resolved.dest.display()),
            )
        })?;
        &real
    };
    if marker != MarkerStatus::Matches {
        let m = Marker {
            preset: preset.name.clone(),
            source: resolved.source.clone(),
            created: jiff::Timestamp::now().to_string(),
        };
        dest.write_marker(&m)
            .map_err(|e| stop(Outcome::Aborted, format!("cannot write {MARKER_NAME}: {e}")))?;
    }

    // Folders made read-only by an earlier finalize must accept changes again.
    if let Err(e) = dest.make_writable(None) {
        warn(stats, emit, format!("destination root: {e}"));
    }
    for e in dst
        .entries
        .iter()
        .filter(|e| e.kind == Kind::Dir && e.mode & 0o200 == 0)
    {
        if let Err(err) = dest.make_writable(Some(&e.rel)) {
            warn(stats, emit, format!("{}: {err}", e.rel));
        }
    }
    for t in &plan.temp_cleanup {
        let _ = dest.remove_nondir(t);
    }
    for (from, to) in &plan.renames {
        if let Err(e) = dest.rename(from, to) {
            error(stats, emit, to.as_str(), format!("rename from {from}: {e}"));
        }
    }
    for c in &plan.clear {
        if let Err(e) = dest.remove_nondir(c) {
            error(stats, emit, c.as_str(), e);
        }
    }

    let mut blocked: Vec<&RelPath> = Vec::new();
    if allow_deletes {
        for d in &plan.replace_trees {
            match remove(dest, d) {
                Ok(()) => stats.deleted += 1,
                Err(e) => error(stats, emit, d.rel.as_str(), e),
            }
        }
    } else {
        for root in &plan.replace_roots {
            error(
                stats,
                emit,
                root.as_str(),
                "a destination folder must be deleted to make room, and deletions are not allowed this run",
            );
            blocked.push(root);
        }
    }
    let is_blocked = |r: &RelPath| blocked.iter().any(|b| r.starts_with(b));

    for m in &plan.mkdirs {
        if !is_blocked(m)
            && let Err(e) = dest.mkdir(m)
        {
            error(stats, emit, m.as_str(), e);
        }
    }

    let mut was_interrupted = false;
    for c in &plan.copies {
        if cancel.load(Ordering::Relaxed) {
            was_interrupted = true;
            break;
        }
        if is_blocked(&c.rel) {
            continue;
        }
        emit(Event::FileStart {
            path: c.rel.to_string(),
            size: c.size,
        });
        let mut file = match fs::File::open(resolved.source.join(c.rel.as_path())) {
            Ok(f) => f,
            Err(e) => {
                error(stats, emit, c.rel.as_str(), e);
                continue;
            }
        };
        let mut pending = 0u64;
        let mut last = Instant::now();
        let result = dest.copy_file(
            &mut file,
            &c.rel,
            &mut |n| {
                pending += n;
                if last.elapsed() >= Duration::from_millis(50) {
                    emit(Event::FileProgress { bytes: pending });
                    pending = 0;
                    last = Instant::now();
                }
            },
            cancel,
        );
        if pending > 0 {
            emit(Event::FileProgress { bytes: pending });
        }
        match result {
            Ok(report) => {
                for w in report.warnings {
                    warn(stats, emit, format!("{}: {w}", c.rel));
                }
                stats.copied_files += 1;
                stats.copied_bytes += c.size;
                emit(Event::FileDone {
                    path: c.rel.to_string(),
                });
            }
            Err(_) if cancel.load(Ordering::Relaxed) => {
                was_interrupted = true;
                break;
            }
            Err(e) => error(stats, emit, c.rel.as_str(), e),
        }
    }
    if was_interrupted {
        return Err(interrupted());
    }
    for l in &plan.links {
        if !is_blocked(&l.rel)
            && let Err(e) = dest.symlink(&l.target, &l.rel, l.mtime)
        {
            error(stats, emit, l.rel.as_str(), e);
        }
    }

    let mut deletions_skipped = false;
    if !plan.deletes.is_empty() {
        if allow_deletes && stats.errors.is_empty() {
            emit(Event::Deleting {
                total: plan.deletes.len() as u64,
            });
            for d in &plan.deletes {
                if cancel.load(Ordering::Relaxed) {
                    return Err(interrupted());
                }
                match remove(dest, d) {
                    Ok(()) => {
                        stats.deleted += 1;
                        emit(Event::Deleted {
                            path: d.rel.to_string(),
                        });
                    }
                    Err(e) => error(stats, emit, d.rel.as_str(), e),
                }
            }
        } else {
            deletions_skipped = true;
            let why = if allow_deletes {
                "some files could not be copied"
            } else {
                "not allowed this run"
            };
            warn(
                stats,
                emit,
                format!("{} deletion(s) skipped: {why}", plan.deletes.len()),
            );
        }
    }

    for d in &plan.dirs {
        if !is_blocked(&d.rel)
            && let Err(e) = dest.set_dir_attrs(&d.rel, d.mode, d.mtime)
        {
            warn(stats, emit, format!("{}: {e}", d.rel));
        }
    }

    stats.outcome = if !stats.errors.is_empty() {
        Outcome::Errors
    } else if deletions_skipped {
        Outcome::DeletionsSkipped
    } else {
        Outcome::Ok
    };
    Ok(())
}
