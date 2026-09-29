//! Scan → plan → decide → execute (spec §6). Runs inside the worker process,
//! or in-process in tests. Emits `Event`s and asks for one `Decision`.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::MARKER_NAME;
use crate::config::Preset;
use crate::dest::{Dest, DestOps, Marker, SimulatedDest, Source, temp_rel};
use crate::plan::{self, Pins, Plan, Replace, Totals, Volume, name_key};
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
    /// Other filesystems mounted inside the destination, left alone.
    pub dest_mounts: u64,
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
    let replaced = plan
        .replaces
        .iter()
        .filter(|r| r.destructive)
        .flat_map(|r| &r.old);
    for d in replaced.chain(&plan.deletes) {
        item(PlanAction::Delete, d.rel.to_string());
    }
}

/// Space the destination must have free before copying: every copy is
/// written in full (at the size it will occupy there) before the file it
/// overwrites is released, and staged replacements keep the old version
/// until the end (AUD-H4).
pub fn needed_bytes(plan: &Plan, vol: Volume, block: u64) -> u64 {
    const PER_FILE: i128 = 4096;
    const RESERVE: i128 = 32 << 20;
    if plan.copies.is_empty() {
        return 0;
    }
    let block = block.max(512) as i128;
    let round = |n: u64| (n as i128 + block - 1) / block * block;
    let (mut used, mut peak) = (0i128, 0i128);
    for c in &plan.copies {
        // Holes and compression survive the copy only on APFS.
        used += round(if vol.sparse { c.alloc } else { c.size }) + PER_FILE;
        peak = peak.max(used);
        used -= c.frees as i128;
    }
    (peak + RESERVE) as u64
}

/// Puts a replacement in place of what it displaces. A non-folder is renamed
/// straight over the old entry. Otherwise the old version is first renamed
/// aside, so the swap never leaves both half there, and then removed entry
/// by entry while each is still the one the scan saw. Returns false (and
/// changes nothing) when the old entry changed since the scan; `leftover`
/// is the set-aside name if part of the old version had to stay.
fn publish(
    dest: &dyn DestOps,
    r: &Replace,
    staged: &RelPath,
    leftover: &mut Option<RelPath>,
) -> std::io::Result<bool> {
    if r.atomic() {
        return dest.replace(staged, &r.rel, r.old[0].ino);
    }
    // Deepest first, so the displaced entry itself comes last.
    let root = r.old.last().expect("a replacement displaces something");
    let Some(aside) = dest.set_aside(&root.rel, root.ino)? else {
        return Ok(false);
    };
    if let Err(e) = dest.rename(staged, &r.rel) {
        let _ = dest.rename(&aside, &root.rel);
        return Err(e);
    }
    for o in &r.old {
        let rest = &o.rel.as_str()[root.rel.as_str().len()..];
        let at = RelPath::new(&format!("{aside}{rest}")).expect("a set-aside path is valid");
        if !dest.remove_if(&at, o.kind, o.ino).unwrap_or(false) {
            *leftover = Some(aside);
            break;
        }
    }
    Ok(true)
}

/// On interrupt: drop half-built replacements and put back the folder
/// modes this run loosened (AUD-L4). The old entries were never touched.
fn abandon(
    dest: &dyn DestOps,
    staged: &[(&Replace, RelPath)],
    modes: &[(Option<RelPath>, u32)],
) -> Stop {
    for (_, t) in staged {
        let _ = dest.remove_temp(t);
    }
    for (rel, mode) in modes {
        let _ = dest.set_mode(rel.as_ref(), *mode);
    }
    Stop {
        outcome: Outcome::Interrupted,
        message: None,
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
    let vol = preflight::volume(&resolved.dest);
    let dst = scan::scan_dest(&resolved.dest, vol.xattrs, cancel).map_err(|e| {
        if cancel.load(Ordering::Relaxed) {
            interrupted()
        } else {
            stop(
                Outcome::PreflightFailed,
                format!("cannot scan destination: {e}"),
            )
        }
    })?;

    // An unreadable source path is an error, and it is pinned: its
    // destination copy is kept rather than taken for extraneous (AUD-M2).
    for (p, m) in &src.errors {
        error(stats, emit, p, m);
    }
    for n in &src.skipped_names {
        warn(
            stats,
            emit,
            format!("{n}: name is not valid UTF-8; skipped"),
        );
    }
    for (p, m) in &dst.errors {
        warn(stats, emit, format!("destination {p}: {m}"));
    }
    for m in &src.mounts {
        warn(
            stats,
            emit,
            format!("{m}: another filesystem is mounted here; not backed up"),
        );
    }
    if src.skipped_special > 0 {
        warn(
            stats,
            emit,
            format!(
                "{} special file(s) (sockets, pipes, devices) skipped",
                src.skipped_special
            ),
        );
    }
    for m in &dst.mounts {
        warn(
            stats,
            emit,
            format!(
                "destination {m} is another filesystem mounted in the backup folder; left alone"
            ),
        );
    }

    let keep: Vec<RelPath> = src
        .pins
        .iter()
        .chain(&dst.pins)
        .chain(&dst.mounts)
        .cloned()
        .collect();
    let pins = Pins {
        keep: &keep,
        mounts: &dst.mounts,
    };
    let plan = plan::build(&src.entries, &dst.entries, &dst.temp_files, vol, pins);
    for (skipped, kept) in &plan.collisions {
        error(
            stats,
            emit,
            skipped.as_str(),
            format!("name collides with {kept} on a case-insensitive destination; skipped"),
        );
    }
    for (b, why) in &plan.blocked {
        error(stats, emit, b.as_str(), why);
    }
    let marker = preflight::marker_status(&resolved.dest, &preset.name, &resolved.source);
    let free = preflight::free_space(&resolved.dest).ok();
    let needed = needed_bytes(
        &plan,
        vol,
        preflight::block_size(&resolved.dest).unwrap_or(4096),
    );
    let destroyed = || {
        plan.deletes.iter().chain(
            plan.replaces
                .iter()
                .filter(|r| r.destructive)
                .flat_map(|r| &r.old),
        )
    };
    let mut largest: Vec<(String, u64)> = destroyed()
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
        skipped_mounts: src.mounts.len() as u64,
        dest_mounts: dst.mounts.len() as u64,
        collisions: plan.collisions.len() as u64,
        scan_errors: (src.errors.len() + dst.errors.len()) as u64,
        case_insensitive: vol.case_insensitive,
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
    let mut allow_deletes = allow_deletes;
    if cancel.load(Ordering::Relaxed) {
        return Err(interrupted());
    }
    if marker.needs_adoption() && !adopt {
        return Err(stop(
            Outcome::Aborted,
            format!(
                "{} is not a bupr folder for this preset (no matching {MARKER_NAME}); refusing to touch it",
                resolved.dest.display()
            ),
        ));
    }

    // Another preset's backup (its own .bupr-dest) inside this destination is
    // never ours to delete.
    let displaced = plan.replaces.iter().flat_map(|r| &r.old);
    for d in plan.deletes.iter().chain(displaced) {
        if d.rel.file_name() == MARKER_NAME
            && let Some(foreign) = d.rel.parent()
        {
            error(
                stats,
                emit,
                foreign.as_str(),
                "holds another bupr backup (.bupr-dest); not deleting it — check your presets' destinations",
            );
            allow_deletes = false;
        }
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
    let source = Source::open(&resolved.source).map_err(|e| {
        stop(
            Outcome::PreflightFailed,
            format!("cannot open {}: {e}", resolved.source.display()),
        )
    })?;
    // Folders made read-only by an earlier finalize must accept changes
    // again. Their modes are put back at the end (AUD-L4).
    let mut modes: Vec<(Option<RelPath>, u32)> = Vec::new();
    match dest.make_writable(None) {
        Ok(Some(m)) => modes.push((None, m)),
        Ok(None) => {}
        Err(e) => warn(stats, emit, format!("destination root: {e}")),
    }
    if marker != MarkerStatus::Matches {
        let m = Marker {
            preset: preset.name.clone(),
            source: resolved.source.clone(),
            created: jiff::Timestamp::now().to_string(),
        };
        if let Err(e) = dest.write_marker(&m) {
            let _ = abandon(dest, &[], &modes);
            return Err(stop(
                Outcome::Aborted,
                format!("cannot write {MARKER_NAME}: {e}"),
            ));
        }
    }
    for e in dst
        .entries
        .iter()
        .filter(|e| e.kind == Kind::Dir && e.mode & 0o200 == 0)
    {
        match dest.make_writable(Some(&e.rel)) {
            Ok(Some(m)) => modes.push((Some(e.rel.clone()), m)),
            Ok(None) => {}
            Err(err) => warn(stats, emit, format!("{}: {err}", e.rel)),
        }
    }
    for t in &plan.temp_cleanup {
        if let Err(e) = dest.remove_temp(t)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            warn(stats, emit, format!("{t}: {e}"));
        }
    }
    let mut staged: Vec<(&Replace, RelPath)> = Vec::new();
    for (from, to) in &plan.renames {
        if cancel.load(Ordering::Relaxed) {
            return Err(abandon(dest, &staged, &modes));
        }
        if let Err(e) = dest.rename(from, to) {
            error(stats, emit, to.as_str(), format!("rename from {from}: {e}"));
        }
    }

    // Replacements are built under a temporary name and only take the old
    // entry's place once complete (AUD-H3).
    let mut blocked: Vec<RelPath> = Vec::new();
    for r in &plan.replaces {
        if r.destructive && !allow_deletes {
            error(
                stats,
                emit,
                r.rel.as_str(),
                "what is in the way in the destination must be deleted to make room, and deletions are not allowed this run",
            );
            blocked.push(r.rel.clone());
            continue;
        }
        match temp_rel(&r.rel) {
            Ok(t) => staged.push((r, t)),
            Err(e) => {
                error(stats, emit, r.rel.as_str(), e);
                blocked.push(r.rel.clone());
            }
        }
    }
    let is_blocked = |r: &RelPath, blocked: &[RelPath]| blocked.iter().any(|b| r.starts_with(b));
    let place = |rel: &RelPath| -> RelPath {
        for (r, t) in &staged {
            if rel.starts_with(&r.rel) {
                let rest = &rel.as_str()[r.rel.as_str().len()..];
                return RelPath::new(&format!("{t}{rest}")).expect("a staged path is valid");
            }
        }
        rel.clone()
    };

    for m in &plan.mkdirs {
        if cancel.load(Ordering::Relaxed) {
            return Err(abandon(dest, &staged, &modes));
        }
        if !is_blocked(m, &blocked)
            && let Err(e) = dest.mkdir(&place(m))
        {
            error(stats, emit, m.as_str(), e);
        }
    }

    for c in &plan.copies {
        if cancel.load(Ordering::Relaxed) {
            return Err(abandon(dest, &staged, &modes));
        }
        if is_blocked(&c.rel, &blocked) {
            continue;
        }
        emit(Event::FileStart {
            path: c.rel.to_string(),
            size: c.size,
        });
        let mut file = match source.open_file(&c.rel, c.ino) {
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
            &place(&c.rel),
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
                return Err(abandon(dest, &staged, &modes));
            }
            Err(e) => error(stats, emit, c.rel.as_str(), e),
        }
    }
    for l in &plan.links {
        if !is_blocked(&l.rel, &blocked)
            && let Err(e) = dest.symlink(&l.target, &place(&l.rel), l.mtime)
        {
            error(stats, emit, l.rel.as_str(), e);
        }
    }

    for (r, t) in &staged {
        if cancel.load(Ordering::Relaxed) {
            return Err(abandon(dest, &staged, &modes));
        }
        let incomplete = stats
            .errors
            .iter()
            .any(|e| RelPath::new(&e.path).is_ok_and(|p| p.starts_with(&r.rel)));
        let mut leftover = None;
        let outcome = if incomplete {
            Ok(false)
        } else {
            publish(dest, r, t, &mut leftover)
        };
        match outcome {
            Ok(true) => {
                if let Some(aside) = leftover {
                    warn(
                        stats,
                        emit,
                        format!(
                            "{}: part of the old version changed since the scan; left in {aside} until the next run",
                            r.rel
                        ),
                    );
                } else if r.destructive {
                    stats.deleted += r.old.len() as u64;
                }
                continue;
            }
            Ok(false) if incomplete => warn(
                stats,
                emit,
                format!(
                    "{}: the new version is incomplete; the old one is kept",
                    r.rel
                ),
            ),
            Ok(false) => error(
                stats,
                emit,
                r.rel.as_str(),
                "changed since the scan; not replaced",
            ),
            Err(e) => error(stats, emit, r.rel.as_str(), e),
        }
        let _ = dest.remove_temp(t);
        blocked.push(r.rel.clone());
    }

    // Unrelated deletions go ahead even when something above failed: pinned
    // paths were never planned for deletion, and every entry is re-checked.
    let mut deletions_skipped = false;
    if !plan.deletes.is_empty() {
        if allow_deletes {
            emit(Event::Deleting {
                total: plan.deletes.len() as u64,
            });
            for d in &plan.deletes {
                if cancel.load(Ordering::Relaxed) {
                    return Err(abandon(dest, &[], &modes));
                }
                match dest.remove_if(&d.rel, d.kind, d.ino) {
                    Ok(true) => {
                        stats.deleted += 1;
                        emit(Event::Deleted {
                            path: d.rel.to_string(),
                        });
                    }
                    Ok(false) => warn(
                        stats,
                        emit,
                        format!("{}: changed since the scan; not deleted", d.rel),
                    ),
                    Err(e) => error(stats, emit, d.rel.as_str(), e),
                }
            }
        } else {
            deletions_skipped = true;
            warn(
                stats,
                emit,
                format!(
                    "{} deletion(s) skipped: not allowed this run",
                    plan.deletes.len()
                ),
            );
        }
    }

    for d in &plan.dirs {
        if !is_blocked(&d.rel, &blocked)
            && let Err(e) = dest.set_dir_attrs(&d.rel, d.mode, d.mtime)
        {
            warn(stats, emit, format!("{}: {e}", d.rel));
        }
    }
    // Folders the finalize pass does not own get their old mode back.
    let finalized: HashSet<String> = plan
        .dirs
        .iter()
        .map(|d| name_key(d.rel.as_str(), vol.case_insensitive))
        .collect();
    for (rel, mode) in &modes {
        let owned = rel
            .as_ref()
            .is_some_and(|r| finalized.contains(&name_key(r.as_str(), vol.case_insensitive)));
        // Gone, or replaced by a file or link: nothing to restore.
        let gone = |e: &std::io::Error| {
            e.kind() == std::io::ErrorKind::NotFound
                || matches!(e.raw_os_error(), Some(libc::ENOTDIR | libc::ELOOP))
        };
        if !owned
            && let Err(e) = dest.set_mode(rel.as_ref(), *mode)
            && !gone(&e)
        {
            let name = rel.as_ref().map_or(".".to_string(), |r| r.to_string());
            warn(stats, emit, format!("{name}: {e}"));
        }
    }
    if let Err(e) = dest.sync() {
        warn(stats, emit, format!("cannot flush the destination: {e}"));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::CopyItem;

    fn copy(size: u64, alloc: u64, frees: u64) -> CopyItem {
        CopyItem {
            rel: RelPath::new("f").unwrap(),
            size,
            alloc,
            ino: 0,
            frees,
        }
    }

    #[test]
    fn needed_space_counts_the_temp_copy_beside_the_file_it_replaces() {
        const MB: u64 = 1 << 20;
        const EXTRA: u64 = (32 << 20) + 4096;
        let apfs = Volume {
            sparse: true,
            ..Volume::default()
        };
        assert_eq!(needed_bytes(&Plan::default(), apfs, 4096), 0);
        // Rewriting a 20 MB file needs 20 MB free, not 0.
        let one = Plan {
            copies: vec![copy(20 * MB, 20 * MB, 20 * MB)],
            ..Plan::default()
        };
        assert_eq!(needed_bytes(&one, apfs, 4096), 20 * MB + EXTRA);
        // Two rewrites in a row: the first file's old copy is freed first.
        let two = Plan {
            copies: vec![
                copy(20 * MB, 20 * MB, 20 * MB),
                copy(20 * MB, 20 * MB, 20 * MB),
            ],
            ..Plan::default()
        };
        assert_eq!(needed_bytes(&two, apfs, 4096), 20 * MB + EXTRA + 4096);
        // A sparse image counts at its allocation on APFS, in full elsewhere.
        let sparse = Plan {
            copies: vec![copy(100 * MB, MB, 0)],
            ..Plan::default()
        };
        assert_eq!(needed_bytes(&sparse, apfs, 4096), MB + EXTRA);
        assert_eq!(
            needed_bytes(&sparse, Volume::default(), 4096),
            100 * MB + EXTRA
        );
        // Small files take whole blocks.
        let small = Plan {
            copies: vec![copy(1, 1, 0)],
            ..Plan::default()
        };
        assert_eq!(
            needed_bytes(&small, Volume::default(), 1 << 17),
            (1 << 17) + EXTRA
        );
    }
}
