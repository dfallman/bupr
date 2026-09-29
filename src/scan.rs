//! Read-only walks of the source (filtered, spec §5) and the destination.
//! Nothing in this module opens a file for writing (spec §3.2).

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

use crate::relpath::RelPath;
use crate::rules::{Decision, Filter, Reason, Siblings};
use crate::{MARKER_NAME, TMP_PREFIX};

/// Extended attributes the system maintains on its own (or that cannot be
/// copied by an ordinary process). They are left out of change detection and
/// of the "not copied" warnings.
pub const VOLATILE_XATTRS: &[&str] = &[
    "com.apple.provenance",
    "com.apple.quarantine",
    "com.apple.macl",
    "com.apple.lastuseddate#PS",
    "com.apple.rootless",
    "com.apple.decmpfs",
];

pub fn is_volatile_xattr(name: &OsStr) -> bool {
    VOLATILE_XATTRS
        .iter()
        .any(|v| v.as_bytes() == name.as_bytes())
}

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
    /// Bytes allocated on disk (less than `size` for sparse or compressed files).
    pub alloc: u64,
    /// Seconds since the epoch.
    pub mtime: i64,
    pub mtime_nsec: u32,
    pub mode: u32,
    pub link_target: Option<PathBuf>,
    /// Inode number at scan time. Source files are re-checked against it
    /// when opened for copying, destination entries before removal.
    pub ino: u64,
    /// Fingerprint of a file's extended attributes (0 when it has none, or
    /// when it was not computed).
    pub xattrs: u64,
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
    /// Directories on another filesystem, not descended into.
    pub mounts: Vec<RelPath>,
    /// Names that are not valid UTF-8, which the mirror cannot represent.
    pub skipped_names: Vec<String>,
    pub errors: Vec<(String, String)>,
    /// Paths that could not be read. Their destination copies are kept.
    pub pins: Vec<RelPath>,
}

#[derive(Debug, Default)]
pub struct DestScan {
    pub entries: Vec<Entry>,
    /// Leftovers of an earlier run (`.bupr-tmp-*`), not descended into.
    pub temp_files: Vec<RelPath>,
    /// Directories on another filesystem, never descended into or deleted.
    pub mounts: Vec<RelPath>,
    pub errors: Vec<(String, String)>,
    /// Paths that could not be read, so what is below them is unknown.
    pub pins: Vec<RelPath>,
}

/// Names and values of a file's extended attributes, hashed.
fn xattr_fingerprint(path: &Path) -> u64 {
    let Ok(names) = xattr::list(path) else {
        return 0;
    };
    let mut names: Vec<_> = names.filter(|n| !is_volatile_xattr(n)).collect();
    if names.is_empty() {
        return 0;
    }
    names.sort();
    let mut h = DefaultHasher::new();
    for n in names {
        n.hash(&mut h);
        xattr::get(path, &n).ok().flatten().hash(&mut h);
    }
    h.finish() | 1
}

fn entry(
    rel: RelPath,
    kind: Kind,
    meta: &fs::Metadata,
    link_target: Option<PathBuf>,
    xattrs: u64,
) -> Entry {
    let file = kind == Kind::File;
    Entry {
        rel,
        kind,
        size: if file { meta.len() } else { 0 },
        alloc: if file { meta.blocks() * 512 } else { 0 },
        mtime: meta.mtime(),
        mtime_nsec: meta.mtime_nsec() as u32,
        mode: meta.mode() & 0o7777,
        link_target,
        ino: meta.ino(),
        xattrs,
    }
}

fn label(rel: Option<&RelPath>) -> String {
    rel.map_or_else(|| ".".to_string(), |r| r.to_string())
}

fn interrupted() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "interrupted")
}

/// Sorted UTF-8 names in `dir`, and the others (lossy) that were skipped.
fn read_names(dir: &Path, rel: Option<&RelPath>) -> io::Result<(Vec<String>, Vec<String>)> {
    let mut names = Vec::new();
    let mut skipped = Vec::new();
    for e in fs::read_dir(dir)? {
        match e?.file_name().into_string() {
            Ok(n) => names.push(n),
            Err(raw) => skipped.push(format!("{}/{}", label(rel), raw.to_string_lossy())),
        }
    }
    names.sort();
    Ok((names, skipped))
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
        let names = match (read_names(&dir_abs, dir_rel.as_ref()), &dir_rel) {
            (Ok((names, skipped)), _) => {
                out.skipped_names.extend(skipped);
                names
            }
            // Nothing is known about the source without its top level.
            (Err(e), None) => return Err(e),
            (Err(e), Some(rel)) => {
                out.errors.push((rel.to_string(), e.to_string()));
                out.pins.push(rel.clone());
                continue;
            }
        };
        let sibs = Siblings::new(&names);
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
                    out.pins.push(rel);
                    continue;
                }
            };
            let ft = meta.file_type();
            let is_dir = ft.is_dir();
            if let Decision::Exclude(reason) = filter.decide(&rel, is_dir, &sibs, dir_excluded) {
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
                    out.mounts.push(rel);
                    continue;
                }
                out.entries
                    .push(entry(rel.clone(), Kind::Dir, &meta, None, 0));
                stack.push((abs, Some(rel), false));
            } else if ft.is_file() {
                if filter.is_secret(&rel) {
                    out.secret_files += 1;
                }
                files += 1;
                bytes += meta.len();
                progress(files, bytes);
                let xattrs = xattr_fingerprint(&abs);
                out.entries
                    .push(entry(rel, Kind::File, &meta, None, xattrs));
            } else if ft.is_symlink() {
                match fs::read_link(&abs) {
                    Ok(t) => {
                        files += 1;
                        progress(files, bytes);
                        out.entries
                            .push(entry(rel, Kind::Symlink, &meta, Some(t), 0));
                    }
                    Err(e) => {
                        out.errors.push((rel.to_string(), e.to_string()));
                        out.pins.push(rel);
                    }
                }
            } else {
                out.skipped_special += 1;
            }
        }
    }
    add_missing_ancestors(root, &mut out);
    out.entries.sort_by(|a, b| a.rel.cmp(&b.rel));
    out.mounts.sort();
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
            Ok(m) if m.is_dir() => out.entries.push(entry(rel, Kind::Dir, &m, None, 0)),
            Ok(_) => {}
            Err(e) => {
                out.errors.push((rel.to_string(), e.to_string()));
                out.pins.push(rel);
            }
        }
    }
}

/// Walks the destination. `fingerprint` computes each file's extended
/// attribute fingerprint, for volumes where they are compared.
pub fn scan_dest(root: &Path, fingerprint: bool, cancel: &AtomicBool) -> io::Result<DestScan> {
    let mut out = DestScan::default();
    let dev = match fs::symlink_metadata(root) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e),
        Ok(m) if !m.is_dir() => {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                format!("{} is not a directory", root.display()),
            ));
        }
        Ok(m) => m.dev(),
    };
    let mut stack: Vec<(PathBuf, Option<RelPath>)> = vec![(root.to_path_buf(), None)];
    while let Some((dir_abs, dir_rel)) = stack.pop() {
        if cancel.load(Ordering::Relaxed) {
            return Err(interrupted());
        }
        let names = match read_names(&dir_abs, dir_rel.as_ref()) {
            Ok((names, skipped)) => {
                // Such a name can never be matched or replaced, so it stays.
                for s in skipped {
                    out.errors
                        .push((s, "name is not valid UTF-8; left alone".to_string()));
                }
                names
            }
            Err(e) => {
                out.errors.push((label(dir_rel.as_ref()), e.to_string()));
                match dir_rel {
                    Some(rel) => out.pins.push(rel),
                    None => return Err(e),
                }
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
                    out.pins.push(rel);
                    continue;
                }
            };
            let ft = meta.file_type();
            if ft.is_dir() && meta.dev() != dev {
                out.mounts.push(rel);
            } else if name.starts_with(TMP_PREFIX) {
                out.temp_files.push(rel);
            } else if ft.is_dir() {
                out.entries
                    .push(entry(rel.clone(), Kind::Dir, &meta, None, 0));
                stack.push((abs, Some(rel)));
            } else if ft.is_symlink() {
                let target = fs::read_link(&abs).ok();
                out.entries
                    .push(entry(rel, Kind::Symlink, &meta, target, 0));
            } else {
                // Regular files, and anything special, which a mirror removes like a file.
                let xattrs = if fingerprint && ft.is_file() {
                    xattr_fingerprint(&abs)
                } else {
                    0
                };
                out.entries
                    .push(entry(rel, Kind::File, &meta, None, xattrs));
            }
        }
    }
    out.entries.sort_by(|a, b| a.rel.cmp(&b.rel));
    out.temp_files.sort();
    out.mounts.sort();
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
        tu::mkdir(&r, ".bupr-tmp-2-staged/inner");
        tu::write(&r, "x/a", b"a");
        tu::write(&r, "target/big", b"b");
        let d = scan_dest(&r, false, &AtomicBool::new(false)).unwrap();
        assert_eq!(rels(&d.entries), ["target", "target/big", "x", "x/a"]);
        assert_eq!(
            d.temp_files,
            vec![
                RelPath::new(".bupr-tmp-2-staged").unwrap(),
                RelPath::new("x/.bupr-tmp-1-a").unwrap()
            ]
        );
        let missing = scan_dest(&r.join("nope"), false, &AtomicBool::new(false)).unwrap();
        assert!(missing.entries.is_empty());
    }

    #[test]
    fn unreadable_folders_are_pinned_not_fatal() {
        let (_t, r) = tmp();
        tu::write(&r, "ok.txt", b"o");
        tu::write(&r, "locked/a.txt", b"a");
        tu::chmod(&r.join("locked"), 0o000);
        let s = scan(&r, &[], &[], &[]);
        let d = scan_dest(&r, false, &AtomicBool::new(false)).unwrap();
        tu::chmod(&r.join("locked"), 0o755);
        assert_eq!(rels(&s.entries), ["locked", "ok.txt"]);
        assert_eq!(s.pins, vec![RelPath::new("locked").unwrap()]);
        assert_eq!(s.errors.len(), 1);
        assert_eq!(d.pins, vec![RelPath::new("locked").unwrap()]);
    }

    #[test]
    fn records_allocation_nanoseconds_and_xattr_fingerprints() {
        let (_t, r) = tmp();
        tu::write(&r, "plain", b"p");
        tu::write(&r, "tagged", b"t");
        tu::set_xattr(&r.join("tagged"), "com.example.tag", b"red");
        let s = scan(&r, &[], &[], &[]);
        let (plain, tagged) = (&s.entries[0], &s.entries[1]);
        assert_eq!(plain.xattrs, 0);
        assert_ne!(tagged.xattrs, 0);
        assert!(plain.alloc > 0);
        tu::set_xattr(&r.join("tagged"), "com.example.tag", b"blue");
        assert_ne!(scan(&r, &[], &[], &[]).entries[1].xattrs, tagged.xattrs);
        tu::set_xattr(&r.join("plain"), "com.apple.quarantine", b"0081;x");
        assert_eq!(
            scan(&r, &[], &[], &[]).entries[0].xattrs,
            0,
            "system-maintained attributes are not part of the fingerprint"
        );
        let d = scan_dest(&r, true, &AtomicBool::new(false)).unwrap();
        assert_eq!(d.entries[1].xattrs, xattr_fingerprint(&r.join("tagged")));
    }
}
