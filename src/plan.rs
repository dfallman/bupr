//! Pure diff of source and destination entries into a Plan (spec §6.1).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

use crate::relpath::RelPath;
use crate::scan::{Entry, Kind};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CopyItem {
    pub rel: RelPath,
    pub size: u64,
    /// Bytes the source occupies on disk.
    pub alloc: u64,
    /// Source inode seen by the scan; the copy refuses a file that no longer matches.
    pub ino: u64,
    /// Bytes allocated to the destination file this copy overwrites, freed
    /// once the copy is renamed over it.
    pub frees: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkItem {
    pub rel: RelPath,
    pub target: PathBuf,
    pub mtime: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeleteItem {
    pub rel: RelPath,
    pub kind: Kind,
    pub size: u64,
    /// Inode seen by the destination scan.
    pub ino: u64,
}

/// A destination entry in the way of a source entry of another kind, or a
/// symlink whose target changed. The new object is built under a temporary
/// name (its `mkdirs`, `copies` and `links` are redirected there) and only
/// takes the place of `old` once it is complete (AUD-H3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Replace {
    pub rel: RelPath,
    pub kind: Kind,
    /// The entries it displaces, deepest first.
    pub old: Vec<DeleteItem>,
    /// Removing `old` destroys backup data (a file, or a folder): it counts
    /// toward the delete limits and needs deletion permission.
    pub destructive: bool,
}

impl Replace {
    /// The new object can be renamed straight over the old one.
    pub fn atomic(&self) -> bool {
        self.kind != Kind::Dir && matches!(&self.old[..], [o] if o.kind != Kind::Dir)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Totals {
    pub copy_files: u64,
    pub copy_bytes: u64,
    pub unchanged_files: u64,
    pub unchanged_bytes: u64,
    /// Entries removed by `deletes` and destructive `replaces`.
    pub delete_entries: u64,
    pub delete_bytes: u64,
}

/// What the destination volume keeps, which decides how files are compared.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Volume {
    pub case_insensitive: bool,
    /// Modification times keep nanoseconds (APFS).
    pub nanos: bool,
    /// Permission bits are stored.
    pub modes: bool,
    /// Extended attributes are stored natively.
    pub xattrs: bool,
    /// Holes and filesystem compression survive a copy (APFS).
    pub sparse: bool,
}

/// Places the plan must not touch.
#[derive(Clone, Copy, Debug, Default)]
pub struct Pins<'a> {
    /// Paths that could not be read (source or destination) and destination
    /// mount points: nothing at, below or above them is deleted or replaced.
    pub keep: &'a [RelPath],
    /// Destination mount points: nothing is written at or below them.
    pub mounts: &'a [RelPath],
}

#[derive(Debug, Default)]
pub struct Plan {
    /// Case-only renames of existing destination entries, parents first.
    pub renames: Vec<(RelPath, RelPath)>,
    pub replaces: Vec<Replace>,
    pub mkdirs: Vec<RelPath>,
    pub copies: Vec<CopyItem>,
    pub links: Vec<LinkItem>,
    /// Extraneous destination entries, deepest first.
    pub deletes: Vec<DeleteItem>,
    pub temp_cleanup: Vec<RelPath>,
    /// Source directories, deepest first, for the finalize pass.
    pub dirs: Vec<Entry>,
    /// (skipped, kept): source names that collide on a case-insensitive destination.
    pub collisions: Vec<(RelPath, RelPath)>,
    /// (source entry, reason): entries not written because a pinned
    /// destination path is in the way.
    pub blocked: Vec<(RelPath, &'static str)>,
    pub totals: Totals,
}

/// Matching key for a path on the destination filesystem. APFS is
/// normalization-insensitive, and when case-insensitive it uses full Unicode
/// case folding (ß≡ss, ς≡σ, µ≡μ, ﬀ≡ff), so the key is the canonical caseless
/// form NFD(fold(NFD(s))). Folding more than the filesystem does is safe: it
/// only turns a copy into a rename or reports a collision.
pub fn name_key(s: &str, case_insensitive: bool) -> String {
    let n: String = s.nfd().collect();
    if case_insensitive {
        caseless::default_case_fold_str(&n).nfd().collect()
    } else {
        n
    }
}

/// `key` is `root` or lies below it (keys are '/'-separated paths).
fn key_under(key: &str, root: &str) -> bool {
    key.strip_prefix(root)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// Unchanged when size and whole-second mtime match, plus whatever else the
/// destination volume keeps (AUD-M1).
fn same_file(s: &Entry, d: &Entry, vol: Volume) -> bool {
    s.size == d.size
        && s.mtime == d.mtime
        && (!vol.nanos || s.mtime_nsec == d.mtime_nsec)
        && (!vol.modes || s.mode == d.mode)
        && (!vol.xattrs || s.xattrs == d.xattrs)
}

fn create(plan: &mut Plan, s: &Entry, frees: u64) {
    match s.kind {
        Kind::Dir => plan.mkdirs.push(s.rel.clone()),
        Kind::File => plan.copies.push(CopyItem {
            rel: s.rel.clone(),
            size: s.size,
            alloc: s.alloc,
            ino: s.ino,
            frees,
        }),
        Kind::Symlink => plan.links.push(LinkItem {
            rel: s.rel.clone(),
            target: s.link_target.clone().unwrap_or_default(),
            mtime: s.mtime,
        }),
    }
}

/// Where a destination entry lives after the planned renames.
fn effective(rel: &RelPath, renamed: &HashMap<String, String>) -> RelPath {
    if renamed.is_empty() {
        return rel.clone();
    }
    let mut orig = String::new();
    let mut cur = String::new();
    for c in rel.components() {
        if !orig.is_empty() {
            orig.push('/');
        }
        orig.push_str(c);
        if let Some(n) = renamed.get(&orig) {
            cur = n.clone();
        } else {
            if !cur.is_empty() {
                cur.push('/');
            }
            cur.push_str(c);
        }
    }
    RelPath::new(&cur).expect("renames preserve path validity")
}

fn deepest_first(v: &mut [DeleteItem]) {
    v.sort_by(|a, b| {
        b.rel
            .depth()
            .cmp(&a.rel.depth())
            .then_with(|| a.rel.cmp(&b.rel))
    });
}

fn delete_item(d: &Entry, renamed: &HashMap<String, String>) -> DeleteItem {
    DeleteItem {
        rel: effective(&d.rel, renamed),
        kind: d.kind,
        size: d.size,
        ino: d.ino,
    }
}

pub fn build(
    source: &[Entry],
    dest: &[Entry],
    temp_files: &[RelPath],
    vol: Volume,
    pins: Pins,
) -> Plan {
    let ci = vol.case_insensitive;
    let key = |r: &RelPath| name_key(r.as_str(), ci);
    let keep: Vec<String> = pins.keep.iter().map(key).collect();
    let mounts: Vec<String> = pins.mounts.iter().map(key).collect();
    // A pinned path, anything below it, and the folders holding it stay.
    let kept = |k: &str| keep.iter().any(|p| key_under(k, p) || key_under(p, k));
    let dest_by_key: HashMap<String, &Entry> = dest.iter().map(|d| (key(&d.rel), d)).collect();
    let mut plan = Plan::default();
    let mut matched: HashSet<&RelPath> = HashSet::new();
    let mut renamed: HashMap<String, String> = HashMap::new();
    // Destination directories being replaced (original path → index in
    // `plan.replaces`), and ones that must stay whole.
    let mut replaced_dirs: Vec<(&RelPath, usize)> = Vec::new();
    let mut kept_dirs: Vec<&RelPath> = Vec::new();
    let mut seen: HashMap<String, RelPath> = HashMap::new();
    let mut skipped: Vec<RelPath> = Vec::new();

    let mut sorted: Vec<&Entry> = source.iter().collect();
    sorted.sort_by(|a, b| a.rel.cmp(&b.rel));
    for s in sorted {
        if skipped.iter().any(|k| s.rel.starts_with(k)) {
            continue;
        }
        let k = key(&s.rel);
        if let Some(kept) = seen.get(&k) {
            plan.collisions.push((s.rel.clone(), kept.clone()));
            skipped.push(s.rel.clone());
            continue;
        }
        seen.insert(k.clone(), s.rel.clone());
        if mounts.iter().any(|m| key_under(&k, m)) {
            plan.blocked.push((
                s.rel.clone(),
                "another filesystem is mounted here in the destination; not backed up",
            ));
            skipped.push(s.rel.clone());
            continue;
        }
        let Some(d) = dest_by_key.get(&k).copied() else {
            if s.kind == Kind::Dir {
                plan.dirs.push(s.clone());
            }
            create(&mut plan, s, 0);
            continue;
        };
        matched.insert(&d.rel);
        let replacing = match (s.kind, d.kind) {
            (Kind::Dir, Kind::Dir) | (Kind::File, Kind::File) | (Kind::File, Kind::Symlink) => {
                false
            }
            (Kind::Symlink, Kind::Symlink) => s.link_target != d.link_target,
            _ => true,
        };
        if replacing && kept(&k) {
            plan.blocked.push((
                s.rel.clone(),
                "the destination entry in the way could not be read; not replaced",
            ));
            skipped.push(s.rel.clone());
            if d.kind == Kind::Dir {
                kept_dirs.push(&d.rel);
            }
            continue;
        }
        if s.kind == Kind::Dir {
            plan.dirs.push(s.clone());
        }
        if name_key(d.rel.file_name(), false) != name_key(s.rel.file_name(), false) {
            let from = RelPath::child(s.rel.parent().as_ref(), d.rel.file_name())
                .expect("dest name is valid");
            plan.renames.push((from, s.rel.clone()));
            renamed.insert(d.rel.as_str().to_string(), s.rel.as_str().to_string());
        }
        match (s.kind, d.kind) {
            (Kind::Dir, Kind::Dir) => {}
            (Kind::File, Kind::File) => {
                if same_file(s, d, vol) {
                    plan.totals.unchanged_files += 1;
                    plan.totals.unchanged_bytes += s.size;
                } else {
                    create(&mut plan, s, d.alloc);
                }
            }
            // Renaming the copied file over a symlink replaces the link itself.
            (Kind::File, Kind::Symlink) => create(&mut plan, s, 0),
            (Kind::Symlink, Kind::Symlink) if !replacing => plan.totals.unchanged_files += 1,
            (_, Kind::Dir) => {
                replaced_dirs.push((&d.rel, plan.replaces.len()));
                plan.replaces.push(Replace {
                    rel: s.rel.clone(),
                    kind: s.kind,
                    old: Vec::new(),
                    destructive: true,
                });
                create(&mut plan, s, 0);
            }
            (_, _) => {
                plan.replaces.push(Replace {
                    rel: s.rel.clone(),
                    kind: s.kind,
                    old: vec![delete_item(d, &renamed)],
                    destructive: d.kind == Kind::File,
                });
                create(&mut plan, s, 0);
            }
        }
    }

    for d in dest {
        if let Some(&(_, i)) = replaced_dirs.iter().find(|(x, _)| d.rel.starts_with(x)) {
            plan.replaces[i].old.push(delete_item(d, &renamed));
        } else if !matched.contains(&d.rel)
            && !kept_dirs.iter().any(|x| d.rel.starts_with(x))
            && !kept(&key(&d.rel))
        {
            plan.deletes.push(delete_item(d, &renamed));
        }
    }
    deepest_first(&mut plan.deletes);
    for r in &mut plan.replaces {
        deepest_first(&mut r.old);
    }
    plan.temp_cleanup = temp_files.iter().map(|t| effective(t, &renamed)).collect();
    plan.dirs.sort_by(|a, b| {
        b.rel
            .depth()
            .cmp(&a.rel.depth())
            .then_with(|| a.rel.cmp(&b.rel))
    });

    let removed = || {
        plan.deletes.iter().chain(
            plan.replaces
                .iter()
                .filter(|r| r.destructive)
                .flat_map(|r| &r.old),
        )
    };
    let delete_entries = removed().count() as u64;
    let delete_bytes = removed()
        .filter(|x| x.kind == Kind::File)
        .map(|x| x.size)
        .sum();
    let t = &mut plan.totals;
    t.copy_files = plan.copies.len() as u64;
    t.copy_bytes = plan.copies.iter().map(|c| c.size).sum();
    t.delete_entries = delete_entries;
    t.delete_bytes = delete_bytes;
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(p: &str) -> RelPath {
        RelPath::new(p).unwrap()
    }
    fn e(rel: &str, kind: Kind, size: u64, mtime: i64) -> Entry {
        Entry {
            rel: r(rel),
            kind,
            size,
            alloc: size,
            mtime,
            mtime_nsec: 0,
            mode: 0o644,
            link_target: None,
            ino: 0,
            xattrs: 0,
        }
    }
    fn f(rel: &str, size: u64, mtime: i64) -> Entry {
        e(rel, Kind::File, size, mtime)
    }
    fn d(rel: &str) -> Entry {
        Entry {
            mode: 0o755,
            ..e(rel, Kind::Dir, 0, 0)
        }
    }
    fn l(rel: &str, target: &str) -> Entry {
        Entry {
            link_target: Some(target.into()),
            ..e(rel, Kind::Symlink, 0, 0)
        }
    }
    fn ci() -> Volume {
        Volume {
            case_insensitive: true,
            ..Volume::default()
        }
    }
    fn build(source: &[Entry], dest: &[Entry], temp: &[RelPath], case_insensitive: bool) -> Plan {
        let vol = Volume {
            case_insensitive,
            ..Volume::default()
        };
        super::build(source, dest, temp, vol, Pins::default())
    }
    fn replaced(p: &Plan) -> Vec<(&str, Vec<&str>, bool)> {
        p.replaces
            .iter()
            .map(|x| {
                (
                    x.rel.as_str(),
                    x.old.iter().map(|o| o.rel.as_str()).collect(),
                    x.destructive,
                )
            })
            .collect()
    }
    fn names<'a>(v: impl IntoIterator<Item = &'a RelPath>) -> Vec<&'a str> {
        v.into_iter().map(|x| x.as_str()).collect()
    }

    #[test]
    fn fresh_destination_creates_everything_parents_first() {
        let p = build(
            &[d("a"), f("a/x", 3, 1), l("a/l", "x"), f("b", 1, 1)],
            &[],
            &[],
            true,
        );
        assert_eq!(names(&p.mkdirs), ["a"]);
        assert_eq!(names(p.copies.iter().map(|c| &c.rel)), ["a/x", "b"]);
        assert_eq!(names(p.links.iter().map(|c| &c.rel)), ["a/l"]);
        assert_eq!((p.totals.copy_files, p.totals.copy_bytes), (2, 4));
        assert!(p.deletes.is_empty() && p.renames.is_empty() && p.replaces.is_empty());
        assert_eq!(names(p.dirs.iter().map(|e| &e.rel)), ["a"]);
    }

    #[test]
    fn unchanged_only_when_size_and_mtime_match() {
        let same = build(&[f("a", 3, 10)], &[f("a", 3, 10)], &[], true);
        assert!(same.copies.is_empty());
        assert_eq!(
            (same.totals.unchanged_files, same.totals.unchanged_bytes),
            (1, 3)
        );
        let newer = build(&[f("a", 3, 11)], &[f("a", 3, 10)], &[], true);
        assert_eq!(names(newer.copies.iter().map(|c| &c.rel)), ["a"]);
        assert_eq!(newer.copies[0].frees, 3);
        let bigger = build(&[f("a", 4, 10)], &[f("a", 3, 10)], &[], true);
        assert_eq!(bigger.copies.len(), 1);
    }

    #[test]
    fn extraneous_entries_are_deleted_deepest_first() {
        let dest = [
            d("old"),
            f("old/x", 5, 1),
            d("old/sub"),
            f("old/sub/y", 2, 1),
            f("z", 1, 1),
        ];
        let p = build(&[], &dest, &[], true);
        assert_eq!(
            names(p.deletes.iter().map(|x| &x.rel)),
            ["old/sub/y", "old/sub", "old/x", "old", "z"]
        );
        assert_eq!((p.totals.delete_entries, p.totals.delete_bytes), (5, 8));
    }

    #[test]
    fn temp_files_are_cleaned_but_not_counted() {
        let p = build(&[], &[], &[r("a/.bupr-tmp-1-x")], true);
        assert_eq!(names(&p.temp_cleanup), ["a/.bupr-tmp-1-x"]);
        assert_eq!(p.totals.delete_entries, 0);
    }

    #[test]
    fn file_replaced_by_directory_is_a_counted_replacement() {
        let p = build(&[d("x"), f("x/c", 1, 1)], &[f("x", 9, 1)], &[], true);
        assert_eq!(replaced(&p), [("x", vec!["x"], true)]);
        assert!(!p.replaces[0].atomic());
        assert_eq!(names(&p.mkdirs), ["x"]);
        assert_eq!(names(p.copies.iter().map(|c| &c.rel)), ["x/c"]);
        assert!(p.deletes.is_empty());
        assert_eq!((p.totals.delete_entries, p.totals.delete_bytes), (1, 9));
    }

    #[test]
    fn directory_replaced_by_file_is_a_gated_tree_removal() {
        let p = build(&[f("x", 1, 1)], &[d("x"), f("x/c", 1, 1)], &[], true);
        assert_eq!(replaced(&p), [("x", vec!["x/c", "x"], true)]);
        assert_eq!(names(p.copies.iter().map(|c| &c.rel)), ["x"]);
        assert!(p.deletes.is_empty());
        assert_eq!(p.totals.delete_entries, 2);
    }

    #[test]
    fn file_replaced_by_symlink_is_atomic_but_counted() {
        let p = build(&[l("x", "t")], &[f("x", 7, 1)], &[], true);
        assert_eq!(replaced(&p), [("x", vec!["x"], true)]);
        assert!(p.replaces[0].atomic());
        assert_eq!(names(p.links.iter().map(|x| &x.rel)), ["x"]);
        assert_eq!((p.totals.delete_entries, p.totals.delete_bytes), (1, 7));
    }

    #[test]
    fn case_only_rename_on_case_insensitive_destination() {
        let p = build(&[f("Readme.md", 3, 1)], &[f("README.md", 3, 1)], &[], true);
        assert_eq!(p.renames, vec![(r("README.md"), r("Readme.md"))]);
        assert!(p.copies.is_empty() && p.deletes.is_empty());
        assert_eq!(p.totals.unchanged_files, 1);
    }

    #[test]
    fn case_only_rename_of_a_directory_rewrites_child_paths() {
        let p = build(
            &[d("Foo"), f("Foo/a", 1, 1)],
            &[d("foo"), f("foo/a", 1, 1), f("foo/old", 1, 1)],
            &[],
            true,
        );
        assert_eq!(p.renames, vec![(r("foo"), r("Foo"))]);
        assert_eq!(names(p.deletes.iter().map(|x| &x.rel)), ["Foo/old"]);
        assert!(p.copies.is_empty());
    }

    #[test]
    fn case_sensitive_destination_treats_case_as_different_names() {
        let p = build(&[f("Readme.md", 3, 1)], &[f("README.md", 3, 1)], &[], false);
        assert!(p.renames.is_empty());
        assert_eq!(names(p.copies.iter().map(|c| &c.rel)), ["Readme.md"]);
        assert_eq!(names(p.deletes.iter().map(|x| &x.rel)), ["README.md"]);
    }

    #[test]
    fn nfd_and_nfc_names_match() {
        // "Björk" decomposed (o + combining diaeresis) vs precomposed ö.
        let nfd = "Bjo\u{308}rk.flac";
        let nfc = "Bj\u{f6}rk.flac";
        for ci in [true, false] {
            let p = build(&[f(nfd, 5, 1)], &[f(nfc, 5, 1)], &[], ci);
            assert!(
                p.renames.is_empty() && p.copies.is_empty() && p.deletes.is_empty(),
                "ci={ci}"
            );
            assert_eq!(p.totals.unchanged_files, 1);
        }
    }

    #[test]
    fn full_case_folding_matches_like_apfs() {
        // APFS folds ß≡ss, ς≡σ, µ≡μ, ﬀ≡ff: a rename between them must be a
        // rename, never copy-then-delete of what is really the same file.
        for (src, dst) in [
            ("straße.txt", "strasse.txt"),
            ("ς.txt", "σ.txt"),
            ("µ.txt", "μ.txt"),
            ("ﬀ.txt", "ff.txt"),
        ] {
            let p = build(&[f(src, 3, 1)], &[f(dst, 3, 1)], &[], true);
            assert_eq!(p.renames, vec![(r(dst), r(src))], "{src} vs {dst}");
            assert!(
                p.deletes.is_empty() && p.copies.is_empty(),
                "{src} vs {dst}"
            );
        }
        let dir = build(
            &[d("Straße"), f("Straße/a", 1, 1)],
            &[d("Strasse"), f("Strasse/a", 1, 1)],
            &[],
            true,
        );
        assert_eq!(dir.renames, vec![(r("Strasse"), r("Straße"))]);
        assert!(dir.deletes.is_empty() && dir.copies.is_empty());
    }

    #[test]
    fn names_colliding_on_a_case_insensitive_destination_are_reported() {
        let p = build(&[f("A.txt", 1, 1), f("a.txt", 2, 1)], &[], &[], true);
        assert_eq!(names(p.copies.iter().map(|c| &c.rel)), ["A.txt"]);
        assert_eq!(p.collisions, vec![(r("a.txt"), r("A.txt"))]);
    }

    #[test]
    fn symlinks_compare_by_target() {
        let changed = build(&[l("l", "new")], &[l("l", "old")], &[], true);
        assert_eq!(replaced(&changed), [("l", vec!["l"], false)]);
        assert!(changed.replaces[0].atomic());
        assert_eq!(names(changed.links.iter().map(|x| &x.rel)), ["l"]);
        assert_eq!(changed.totals.delete_entries, 0);
        let same = build(&[l("l", "t")], &[l("l", "t")], &[], true);
        assert!(same.links.is_empty() && same.replaces.is_empty());
    }

    #[test]
    fn file_over_symlink_is_a_plain_copy() {
        let p = build(&[f("x", 1, 1)], &[l("x", "t")], &[], true);
        assert_eq!(names(p.copies.iter().map(|c| &c.rel)), ["x"]);
        assert!(p.replaces.is_empty() && p.deletes.is_empty());
    }

    #[test]
    fn dirs_are_listed_deepest_first_for_finalize() {
        let p = build(&[d("a"), d("a/b")], &[], &[], true);
        assert_eq!(names(p.dirs.iter().map(|e| &e.rel)), ["a/b", "a"]);
    }

    #[test]
    fn nanoseconds_modes_and_xattrs_count_where_the_volume_keeps_them() {
        let s = Entry {
            mtime_nsec: 5,
            mode: 0o600,
            xattrs: 9,
            ..f("a", 3, 10)
        };
        let d = f("a", 3, 10);
        let apfs = Volume {
            nanos: true,
            modes: true,
            xattrs: true,
            ..ci()
        };
        for (vol, copies) in [(ci(), 0), (apfs, 1)] {
            let p = super::build(
                std::slice::from_ref(&s),
                std::slice::from_ref(&d),
                &[],
                vol,
                Pins::default(),
            );
            assert_eq!(p.copies.len(), copies, "{vol:?}");
        }
        for changed in [
            Entry {
                mtime_nsec: 6,
                ..s.clone()
            },
            Entry {
                mode: 0o644,
                ..s.clone()
            },
            Entry {
                xattrs: 8,
                ..s.clone()
            },
        ] {
            let p = super::build(
                std::slice::from_ref(&s),
                &[changed],
                &[],
                apfs,
                Pins::default(),
            );
            assert_eq!(p.copies.len(), 1);
        }
        let same = super::build(
            std::slice::from_ref(&s),
            std::slice::from_ref(&s),
            &[],
            apfs,
            Pins::default(),
        );
        assert!(same.copies.is_empty());
    }

    #[test]
    fn pinned_paths_are_never_deleted_or_replaced() {
        // "gone" could not be read in the source; "mnt" is another filesystem
        // mounted inside the destination.
        let keep = [r("Gone"), r("mnt")];
        let pins = Pins {
            keep: &keep,
            mounts: &keep[1..],
        };
        let p = super::build(
            &[
                d("keep"),
                f("mnt", 1, 1),
                f("mnt/x", 1, 1),
                f("other", 1, 1),
            ],
            &[
                d("gone"),
                f("gone/a", 1, 1),
                d("keep"),
                f("keep/old", 1, 1),
                d("parent"),
                f("parent/x", 1, 1),
            ],
            &[],
            ci(),
            pins,
        );
        assert_eq!(
            names(p.deletes.iter().map(|x| &x.rel)),
            ["keep/old", "parent/x", "parent"]
        );
        assert_eq!(names(p.copies.iter().map(|c| &c.rel)), ["other"]);
        assert_eq!(names(p.blocked.iter().map(|(b, _)| b)), ["mnt"]);
        let under_mount = Pins {
            keep: &[],
            mounts: &keep[1..],
        };
        let p = super::build(&[d("mnt"), f("mnt/x", 1, 1)], &[], &[], ci(), under_mount);
        assert!(p.mkdirs.is_empty() && p.copies.is_empty() && p.dirs.is_empty());
    }

    #[test]
    fn ancestors_of_a_pin_are_kept_and_replacements_over_a_pin_are_blocked() {
        let keep = [r("a/b/unreadable")];
        let pins = Pins {
            keep: &keep,
            mounts: &[],
        };
        let p = super::build(
            &[f("a", 1, 1)],
            &[d("a"), d("a/b"), d("a/b/unreadable"), f("a/c", 1, 1)],
            &[],
            ci(),
            pins,
        );
        assert!(p.replaces.is_empty() && p.copies.is_empty());
        assert!(p.deletes.is_empty(), "{:?}", p.deletes);
        assert_eq!(names(p.blocked.iter().map(|(b, _)| b)), ["a"]);
    }
}
