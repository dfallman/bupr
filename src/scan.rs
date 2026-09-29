//! Read-only walks of the source (filtered, spec §5) and the destination.
//! Nothing in this module opens a file for writing (spec §3.2).

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

use crate::relpath::RelPath;
use crate::rules::{Decision, Filter, Reason};
use crate::{MARKER_NAME, TMP_PREFIX};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    File,
    Dir,
    Symlink,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub rel: RelPath,
    pub kind: Kind,
    pub size: u64,
    /// Seconds since the epoch (whole seconds; spec §6.1).
    pub mtime: i64,
    pub mode: u32,
    pub link_target: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExcludedEntry {
    pub rel: RelPath,
    pub is_dir: bool,
    pub reason: Reason,
}

#[derive(Debug, Default)]
pub struct SourceScan {
    pub entries: Vec<Entry>,
    /// Top-most excluded entries (children of excluded directories are not listed).
    pub excluded: Vec<ExcludedEntry>,
    pub secret_files: u64,
    pub skipped_special: u64,
    pub skipped_mounts: u64,
    pub errors: Vec<(String, String)>,
}

#[derive(Debug, Default)]
pub struct DestScan {
    pub entries: Vec<Entry>,
    pub temp_files: Vec<RelPath>,
    pub errors: Vec<(String, String)>,
}

fn entry(rel: RelPath, kind: Kind, meta: &fs::Metadata, link_target: Option<PathBuf>) -> Entry {
    Entry {
        rel,
        kind,
        size: if kind == Kind::File { meta.len() } else { 0 },
        mtime: meta.mtime(),
        mode: meta.mode() & 0o7777,
        link_target,
    }
}

fn label(rel: Option<&RelPath>) -> String {
    rel.map_or_else(|| ".".to_string(), |r| r.to_string())
}

fn interrupted() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "interrupted")
}

/// Sorted UTF-8 names in `dir`; other names are reported and skipped.
fn read_names(
    dir: &Path,
    rel: Option<&RelPath>,
    errors: &mut Vec<(String, String)>,
) -> io::Result<Vec<String>> {
    let mut names = Vec::new();
    for e in fs::read_dir(dir)? {
        match e?.file_name().into_string() {
            Ok(n) => names.push(n),
            Err(raw) => errors.push((
                format!("{}/{}", label(rel), raw.to_string_lossy()),
                "name is not valid UTF-8; skipped".to_string(),
            )),
        }
    }
    names.sort();
    Ok(names)
}

pub fn scan_source(
    root: &Path,
    filter: &Filter,
    progress: &mut dyn FnMut(u64, u64),
    cancel: &AtomicBool,
) -> io::Result<SourceScan> {
    let root_meta = fs::metadata(root)?;
    if !root_meta.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            format!("{} is not a directory", root.display()),
        ));
    }
    let dev = root_meta.dev();
    let mut out = SourceScan::default();
    let (mut files, mut bytes) = (0u64, 0u64);
    // (absolute dir, its rel path (None = root), inside an excluded dir?)
    let mut stack: Vec<(PathBuf, Option<RelPath>, bool)> = vec![(root.to_path_buf(), None, false)];
    while let Some((dir_abs, dir_rel, dir_excluded)) = stack.pop() {
        if cancel.load(Ordering::Relaxed) {
            return Err(interrupted());
        }
        let names = match read_names(&dir_abs, dir_rel.as_ref(), &mut out.errors) {
            Ok(n) => n,
            Err(e) => {
                out.errors.push((label(dir_rel.as_ref()), e.to_string()));
                continue;
            }
        };
        for name in &names {
            if dir_rel.is_none() && name == MARKER_NAME {
                continue;
            }
            let rel = match RelPath::child(dir_rel.as_ref(), name) {
                Ok(r) => r,
                Err(e) => {
                    out.errors
                        .push((format!("{}/{name}", label(dir_rel.as_ref())), e.to_string()));
                    continue;
                }
            };
            let abs = dir_abs.join(name);
            let meta = match fs::symlink_metadata(&abs) {
                Ok(m) => m,
                Err(e) => {
                    out.errors.push((rel.to_string(), e.to_string()));
                    continue;
                }
            };
            let ft = meta.file_type();
            let is_dir = ft.is_dir();
            if let Decision::Exclude(reason) = filter.decide(&rel, is_dir, &names, dir_excluded) {
                if !dir_excluded {
                    out.excluded.push(ExcludedEntry {
                        rel: rel.clone(),
                        is_dir,
                        reason,
                    });
                }
                if is_dir && meta.dev() == dev && filter.may_include_below(&rel) {
                    stack.push((abs, Some(rel), true));
                }
                continue;
            }
            if is_dir {
                if meta.dev() != dev {
                    out.skipped_mounts += 1;
                    continue;
                }
                out.entries.push(entry(rel.clone(), Kind::Dir, &meta, None));
                stack.push((abs, Some(rel), false));
            } else if ft.is_file() {
                if filter.is_secret(&rel) {
                    out.secret_files += 1;
                }
                files += 1;
                bytes += meta.len();
                progress(files, bytes);
                out.entries.push(entry(rel, Kind::File, &meta, None));
            } else if ft.is_symlink() {
                match fs::read_link(&abs) {
                    Ok(t) => {
                        files += 1;
                        progress(files, bytes);
                        out.entries.push(entry(rel, Kind::Symlink, &meta, Some(t)));
                    }
                    Err(e) => out.errors.push((rel.to_string(), e.to_string())),
                }
            } else {
                out.skipped_special += 1;
            }
        }
    }
    add_missing_ancestors(root, &mut out);
    out.entries.sort_by(|a, b| a.rel.cmp(&b.rel));
    Ok(out)
}

/// Entries force-included below an excluded directory need their ancestor
/// directories in the plan too.
fn add_missing_ancestors(root: &Path, out: &mut SourceScan) {
    let present: BTreeSet<RelPath> = out.entries.iter().map(|e| e.rel.clone()).collect();
    let mut missing = BTreeSet::new();
    for e in &out.entries {
        let mut p = e.rel.parent();
        while let Some(a) = p {
            if present.contains(&a) {
                break;
            }
            p = a.parent();
            missing.insert(a);
        }
    }
    for rel in missing {
        match fs::symlink_metadata(root.join(rel.as_path())) {
            Ok(m) if m.is_dir() => out.entries.push(entry(rel, Kind::Dir, &m, None)),
            Ok(_) => {}
            Err(e) => out.errors.push((rel.to_string(), e.to_string())),
        }
    }
}

pub fn scan_dest(root: &Path, cancel: &AtomicBool) -> io::Result<DestScan> {
    let mut out = DestScan::default();
    match fs::symlink_metadata(root) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e),
        Ok(m) if !m.is_dir() => {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                format!("{} is not a directory", root.display()),
            ));
        }
        Ok(_) => {}
    }
    let mut stack: Vec<(PathBuf, Option<RelPath>)> = vec![(root.to_path_buf(), None)];
    while let Some((dir_abs, dir_rel)) = stack.pop() {
        if cancel.load(Ordering::Relaxed) {
            return Err(interrupted());
        }
        let names = match read_names(&dir_abs, dir_rel.as_ref(), &mut out.errors) {
            Ok(n) => n,
            Err(e) => {
                out.errors.push((label(dir_rel.as_ref()), e.to_string()));
                continue;
            }
        };
        for name in names {
            if dir_rel.is_none() && name == MARKER_NAME {
                continue;
            }
            let Ok(rel) = RelPath::child(dir_rel.as_ref(), &name) else {
                continue;
            };
            let abs = dir_abs.join(&name);
            let meta = match fs::symlink_metadata(&abs) {
                Ok(m) => m,
                Err(e) => {
                    out.errors.push((rel.to_string(), e.to_string()));
                    continue;
                }
            };
            let ft = meta.file_type();
            if name.starts_with(TMP_PREFIX) && ft.is_file() {
                out.temp_files.push(rel);
            } else if ft.is_dir() {
                out.entries.push(entry(rel.clone(), Kind::Dir, &meta, None));
                stack.push((abs, Some(rel)));
            } else if ft.is_symlink() {
                let target = fs::read_link(&abs).ok();
                out.entries.push(entry(rel, Kind::Symlink, &meta, target));
            } else {
                // Regular files, and anything special, which a mirror removes like a file.
                out.entries.push(entry(rel, Kind::File, &meta, None));
            }
        }
    }
    out.entries.sort_by(|a, b| a.rel.cmp(&b.rel));
    out.temp_files.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::RulePack;
    use crate::testutil as tu;

    fn own(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }
    fn scan(root: &Path, packs: &[RulePack], include: &[&str], exclude: &[&str]) -> SourceScan {
        let f = Filter::new(packs, &own(include), &own(exclude), &[]).unwrap();
        scan_source(root, &f, &mut |_, _| {}, &AtomicBool::new(false)).unwrap()
    }
    fn rels(s: &[Entry]) -> Vec<&str> {
        s.iter().map(|e| e.rel.as_str()).collect()
    }
    fn tmp() -> (tempfile::TempDir, PathBuf) {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().canonicalize().unwrap();
        (t, p)
    }

    #[test]
    fn walks_and_prunes_rule_matches_without_descending() {
        let (_t, r) = tmp();
        tu::write(&r, "p/Cargo.toml", b"x");
        tu::write(&r, "p/src/main.rs", b"fn main() {}");
        tu::write(&r, "p/target/debug/big", b"0123456789");
        tu::mkdir(&r, "p/target/locked");
        tu::chmod(&r.join("p/target/locked"), 0o000);
        let s = scan(&r, &[RulePack::Dev], &[], &[]);
        tu::chmod(&r.join("p/target/locked"), 0o755);
        assert_eq!(
            rels(&s.entries),
            ["p", "p/Cargo.toml", "p/src", "p/src/main.rs"]
        );
        assert!(
            s.errors.is_empty(),
            "pruned dirs must not be read: {:?}",
            s.errors
        );
        assert_eq!(
            s.excluded,
            vec![ExcludedEntry {
                rel: RelPath::new("p/target").unwrap(),
                is_dir: true,
                reason: Reason::Rule("target/ next to Cargo.toml".into()),
            }]
        );
    }

    #[test]
    fn records_metadata_and_symlinks_without_following() {
        let (_t, r) = tmp();
        tu::write(&r, "a.txt", b"hello");
        tu::chmod(&r.join("a.txt"), 0o640);
        tu::set_mtime(&r.join("a.txt"), 1_600_000_000);
        tu::symlink("/etc", &r, "link");
        let s = scan(&r, &[], &[], &[]);
        let a = s
            .entries
            .iter()
            .find(|e| e.rel.as_str() == "a.txt")
            .unwrap();
        assert_eq!(
            (a.kind, a.size, a.mtime, a.mode),
            (Kind::File, 5, 1_600_000_000, 0o640)
        );
        let l = s.entries.iter().find(|e| e.rel.as_str() == "link").unwrap();
        assert_eq!(l.kind, Kind::Symlink);
        assert_eq!(l.link_target.as_deref(), Some(Path::new("/etc")));
    }

    #[test]
    fn skips_root_marker_and_special_files_and_counts_secrets() {
        let (_t, r) = tmp();
        tu::write(&r, ".bupr-dest", b"{}");
        tu::write(&r, "sub/.bupr-dest", b"x");
        tu::write(&r, "app/.env", b"K=V");
        tu::mkfifo(&r.join("fifo"));
        let s = scan(&r, &[], &[], &[]);
        assert_eq!(
            rels(&s.entries),
            ["app", "app/.env", "sub", "sub/.bupr-dest"]
        );
        assert_eq!(s.skipped_special, 1);
        assert_eq!(s.secret_files, 1);
    }

    #[test]
    fn force_include_inside_excluded_dir_brings_its_ancestors() {
        let (_t, r) = tmp();
        tu::write(&r, "big/keep/x.txt", b"x");
        tu::write(&r, "big/other.bin", b"y");
        let s = scan(&r, &[], &["/big/keep/"], &["/big/"]);
        assert_eq!(rels(&s.entries), ["big", "big/keep", "big/keep/x.txt"]);
    }

    #[test]
    fn reports_progress_and_honours_cancel() {
        let (_t, r) = tmp();
        tu::write(&r, "a", b"12");
        tu::write(&r, "b", b"345");
        let f = Filter::new(&[], &[], &[], &[]).unwrap();
        let mut last = (0, 0);
        scan_source(&r, &f, &mut |n, b| last = (n, b), &AtomicBool::new(false)).unwrap();
        assert_eq!(last, (2, 5));
        let e = scan_source(&r, &f, &mut |_, _| {}, &AtomicBool::new(true)).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::Interrupted);
    }

    #[test]
    fn dest_scan_lists_everything_and_separates_temp_files() {
        let (_t, r) = tmp();
        tu::write(&r, ".bupr-dest", b"{}");
        tu::write(&r, "x/.bupr-tmp-1-a", b"partial");
        tu::write(&r, "x/a", b"a");
        tu::write(&r, "target/big", b"b");
        let d = scan_dest(&r, &AtomicBool::new(false)).unwrap();
        assert_eq!(rels(&d.entries), ["target", "target/big", "x", "x/a"]);
        assert_eq!(d.temp_files, vec![RelPath::new("x/.bupr-tmp-1-a").unwrap()]);
        let missing = scan_dest(&r.join("nope"), &AtomicBool::new(false)).unwrap();
        assert!(missing.entries.is_empty());
    }
}
