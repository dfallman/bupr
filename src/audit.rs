//! `bupr audit` (spec §8.5): what a preset includes, what it skips and why,
//! and large gitignored folders worth excluding. Read-only; `.gitignore` is
//! only ever a hint here.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::AtomicBool;

use crate::config::Preset;
use crate::relpath::RelPath;
use crate::scan::{self, Kind};
use crate::ui::format::{bytes, count, tilde};

pub const HINT_MIN_BYTES: u64 = 100_000_000;

#[derive(Debug, Default)]
pub struct AuditReport {
    pub included_files: u64,
    pub included_bytes: u64,
    /// (reason, bytes), largest first.
    pub excluded: Vec<(String, u64)>,
    pub top_dirs: Vec<(String, u64)>,
    pub hints: Vec<Hint>,
    pub git_missing: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hint {
    pub path: String,
    pub bytes: u64,
    pub suggestion: String,
}

/// Total size of regular files under `path`, not following symlinks or
/// crossing filesystems.
pub fn du(path: &Path) -> u64 {
    let Ok(root) = fs::symlink_metadata(path) else {
        return 0;
    };
    if !root.is_dir() {
        return if root.is_file() { root.len() } else { 0 };
    }
    let dev = root.dev();
    let mut total = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let Ok(m) = fs::symlink_metadata(e.path()) else {
                continue;
            };
            if m.is_dir() && m.dev() == dev {
                stack.push(e.path());
            } else if m.is_file() {
                total += m.len();
            }
        }
    }
    total
}

pub fn audit(preset: &Preset, min_hint_bytes: u64) -> anyhow::Result<AuditReport> {
    let filter = preset.filter().map_err(anyhow::Error::msg)?;
    let root = fs::canonicalize(&preset.source)?;
    let scan = scan::scan_source(&root, &filter, &mut |_, _| {}, &AtomicBool::new(false))?;
    let mut rep = AuditReport::default();

    let mut dirs: HashMap<String, u64> = HashMap::new();
    for e in scan.entries.iter().filter(|e| e.kind == Kind::File) {
        rep.included_files += 1;
        rep.included_bytes += e.size;
        let mut p = e.rel.parent();
        while let Some(a) = p {
            *dirs.entry(a.to_string()).or_default() += e.size;
            p = a.parent();
        }
    }
    let mut top: Vec<(String, u64)> = dirs.iter().map(|(k, v)| (k.clone(), *v)).collect();
    top.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    top.truncate(20);
    rep.top_dirs = top;

    let mut by_reason: BTreeMap<String, u64> = BTreeMap::new();
    for x in &scan.excluded {
        *by_reason.entry(x.reason.label()).or_default() += du(&root.join(x.rel.as_path()));
    }
    rep.excluded = by_reason.into_iter().collect();
    rep.excluded.sort_by_key(|x| std::cmp::Reverse(x.1));

    let excluded_roots: Vec<&RelPath> = scan.excluded.iter().map(|x| &x.rel).collect();
    let repos: Vec<String> = scan
        .entries
        .iter()
        .filter(|e| e.rel.file_name() == ".git")
        .map(|e| e.rel.parent().map(|p| p.to_string()).unwrap_or_default())
        .collect();
    for repo in repos {
        let dir = if repo.is_empty() {
            root.clone()
        } else {
            root.join(&repo)
        };
        let out = Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args([
                "ls-files",
                "-z",
                "--others",
                "--ignored",
                "--exclude-standard",
                "--directory",
            ])
            .output();
        let stdout = match out {
            Ok(o) if o.status.success() => o.stdout,
            Ok(_) => continue,
            Err(_) => {
                rep.git_missing = true;
                break;
            }
        };
        for item in stdout.split(|b| *b == 0).filter(|s| !s.is_empty()) {
            let s = String::from_utf8_lossy(item);
            let Some(d) = s.strip_suffix('/') else {
                continue;
            };
            let rel_s = if repo.is_empty() {
                d.to_string()
            } else {
                format!("{repo}/{d}")
            };
            let Ok(rel) = RelPath::new(&rel_s) else {
                continue;
            };
            if excluded_roots.iter().any(|x| rel.starts_with(x)) {
                continue;
            }
            // Bytes the preset currently backs up under this folder.
            let size = dirs.get(&rel_s).copied().unwrap_or(0);
            if size >= min_hint_bytes {
                rep.hints.push(Hint {
                    suggestion: format!("/{rel_s}/"),
                    path: rel_s,
                    bytes: size,
                });
            }
        }
    }
    rep.hints.sort_by_key(|h| std::cmp::Reverse(h.bytes));
    Ok(rep)
}

pub fn print(preset: &Preset, rep: &AuditReport, home: &Path) {
    println!(
        "{} · {} → {}",
        preset.name,
        tilde(&preset.source, home),
        tilde(&preset.destination, home)
    );
    println!(
        "  backs up   {} files ({})",
        count(rep.included_files),
        bytes(rep.included_bytes)
    );
    if !rep.excluded.is_empty() {
        println!("\n  skipped:");
        for (reason, b) in &rep.excluded {
            println!("    {:>10}  {reason}", bytes(*b));
        }
    }
    println!("\n  largest included folders:");
    for (d, b) in &rep.top_dirs {
        println!("    {:>10}  {d}", bytes(*b));
    }
    if rep.git_missing {
        println!("\n  (git not found; no .gitignore hints)");
    } else if !rep.hints.is_empty() {
        println!("\n  large folders your .gitignore files ignore but bupr still backs up:");
        for h in &rep.hints {
            println!(
                "    {:>10}  {}   → exclude with \"{}\"",
                bytes(h.bytes),
                h.path,
                h.suggestion
            );
        }
        println!("  (these are only hints — gitignored files can matter, e.g. agent docs or .env)");
    }
}
