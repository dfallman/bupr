//! Fixtures for integration tests. Tests may write freely; the shipped
//! binary's write boundary is enforced by clippy and tests/lint_guard.rs.
#![allow(clippy::disallowed_methods, dead_code)]

use std::collections::BTreeMap;
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, UNIX_EPOCH};

use bupr::config::Preset;
use bupr::engine::{self, Decision, Event, PlanSummary, RunOptions, RunStats};
use bupr::preflight::Env;
use bupr::rules::RulePack;

pub struct Fx {
    _t: tempfile::TempDir,
    pub root: PathBuf,
    pub src: PathBuf,
    pub dst: PathBuf,
    pub outside: PathBuf,
    outside_before: BTreeMap<String, String>,
}

impl Fx {
    pub fn new() -> Fx {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().canonicalize().unwrap();
        for d in ["src", "home", "Volumes", "backup", "outside/nested"] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        write(&root, "outside/precious.txt", b"do not touch");
        write(&root, "outside/nested/deep.txt", b"nor this");
        let outside = root.join("outside");
        let outside_before = snapshot(&outside);
        Fx {
            src: root.join("src"),
            dst: root.join("backup/dev"),
            outside,
            outside_before,
            root,
            _t: t,
        }
    }

    pub fn env(&self) -> Env {
        Env {
            home: self.root.join("home"),
            volumes: self.root.join("Volumes"),
        }
    }

    pub fn preset(&self) -> Preset {
        let mut p = Preset::minimal("dev", self.src.clone(), self.dst.clone());
        p.allow_internal = true;
        p.rules = vec![RulePack::Dev];
        p
    }

    pub fn assert_outside_untouched(&self) {
        assert_eq!(
            snapshot(&self.outside),
            self.outside_before,
            "the sentinel outside the destination changed"
        );
    }
}

impl Drop for Fx {
    fn drop(&mut self) {
        // Spec §3.7: every test ends with the sentinel byte-identical.
        if !std::thread::panicking() {
            self.assert_outside_untouched();
        }
        // Read-only directories created by tests would block TempDir cleanup.
        make_all_writable(&self.root);
    }
}

pub fn write(root: &Path, rel: &str, content: &[u8]) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

pub fn symlink(target: &str, root: &Path, rel: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(target, p).unwrap();
}

pub fn set_mtime(path: &Path, secs: u64) {
    fs::File::open(path)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(secs)))
        .unwrap();
}

pub fn chmod(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

pub fn make_all_writable(p: &Path) {
    if let Ok(m) = fs::symlink_metadata(p) {
        if m.is_dir() {
            let _ = fs::set_permissions(p, fs::Permissions::from_mode(0o755));
            if let Ok(rd) = fs::read_dir(p) {
                for e in rd.flatten() {
                    make_all_writable(&e.path());
                }
            }
        } else if m.is_file() {
            let _ = fs::set_permissions(p, fs::Permissions::from_mode(0o644));
        }
    }
}

/// All entries under `root` (relative, sorted), not following symlinks.
pub fn files(root: &Path) -> Vec<String> {
    snapshot(root).into_keys().collect()
}

/// rel path → kind + content hash / link target + mode + mtime.
pub fn snapshot(root: &Path) -> BTreeMap<String, String> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
        let Ok(rd) = fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            let rel = p.strip_prefix(root).unwrap().to_string_lossy().into_owned();
            let m = fs::symlink_metadata(&p).unwrap();
            let desc = if m.file_type().is_symlink() {
                format!("link:{}", fs::read_link(&p).unwrap().display())
            } else if m.is_dir() {
                walk(root, &p, out);
                format!("dir:{:o}", m.mode() & 0o7777)
            } else {
                let mut h = DefaultHasher::new();
                fs::read(&p).unwrap_or_default().hash(&mut h);
                format!(
                    "file:{:x}:{:o}:{}",
                    h.finish(),
                    m.mode() & 0o7777,
                    m.mtime()
                )
            };
            out.insert(rel, desc);
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

pub struct Run {
    pub events: Vec<Event>,
    pub stats: RunStats,
    pub summary: Option<PlanSummary>,
}

pub fn run_with(
    fx: &Fx,
    preset: &Preset,
    opts: &RunOptions,
    decision: Decision,
    cancel: &AtomicBool,
    hook: &mut dyn FnMut(&Event),
) -> Run {
    let mut events = Vec::new();
    let mut summary = None;
    let stats = engine::run(
        preset,
        &fx.env(),
        opts,
        &mut |e| {
            hook(&e);
            events.push(e);
        },
        &mut |s| {
            summary = Some(s.clone());
            decision
        },
        cancel,
    );
    Run {
        events,
        stats,
        summary,
    }
}

pub fn run(fx: &Fx, preset: &Preset) -> Run {
    run_with(
        fx,
        preset,
        &RunOptions::default(),
        Decision::Proceed {
            allow_deletes: true,
            adopt: false,
        },
        &AtomicBool::new(false),
        &mut |_| {},
    )
}
