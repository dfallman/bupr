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
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Totals {
    pub copy_files: u64,
    pub copy_bytes: u64,
    /// Destination bytes that copies will overwrite.
    pub replaced_bytes: u64,
    pub unchanged_files: u64,
    pub unchanged_bytes: u64,
    /// Entries removed by `deletes` and `replace_trees`.
    pub delete_entries: u64,
    pub delete_bytes: u64,
}

#[derive(Debug, Default)]
pub struct Plan {
    /// Case-only renames of existing destination entries, parents first.
    pub renames: Vec<(RelPath, RelPath)>,
    /// Non-directory destination entries in the way of a different kind
    /// (treated like an overwrite; not gated by deletion permission).
    pub clear: Vec<RelPath>,
    /// Destination directories, with contents, deepest first, in the way of
    /// a non-directory. Gated like deletions.
    pub replace_trees: Vec<DeleteItem>,
    /// Source entries that can only be created once `replace_trees` is gone.
    pub replace_roots: Vec<RelPath>,
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
    pub totals: Totals,
}

/// Matching key for a path on the destination filesystem: NFC-normalized
/// (APFS is normalization-insensitive), case-folded when case-insensitive.
pub fn name_key(s: &str, case_insensitive: bool) -> String {
    let n: String = s.nfc().collect();
    if case_insensitive {
        n.to_lowercase()
    } else {
        n
    }
}

fn create(plan: &mut Plan, s: &Entry) {
    match s.kind {
        Kind::Dir => plan.mkdirs.push(s.rel.clone()),
        Kind::File => plan.copies.push(CopyItem {
            rel: s.rel.clone(),
            size: s.size,
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

pub fn build(
    source: &[Entry],
    dest: &[Entry],
    temp_files: &[RelPath],
    case_insensitive: bool,
) -> Plan {
    let ci = case_insensitive;
    let dest_by_key: HashMap<String, &Entry> = dest
        .iter()
        .map(|d| (name_key(d.rel.as_str(), ci), d))
        .collect();
    let mut plan = Plan::default();
    let mut matched: HashSet<&RelPath> = HashSet::new();
    let mut renamed: HashMap<String, String> = HashMap::new();
    let mut replaced_dirs: Vec<&RelPath> = Vec::new();
    let mut seen: HashMap<String, RelPath> = HashMap::new();
    let mut skipped: Vec<RelPath> = Vec::new();

    let mut sorted: Vec<&Entry> = source.iter().collect();
    sorted.sort_by(|a, b| a.rel.cmp(&b.rel));
    for s in sorted {
        if skipped.iter().any(|k| s.rel.starts_with(k)) {
            continue;
        }
        let key = name_key(s.rel.as_str(), ci);
        if let Some(kept) = seen.get(&key) {
            plan.collisions.push((s.rel.clone(), kept.clone()));
            skipped.push(s.rel.clone());
            continue;
        }
        seen.insert(key.clone(), s.rel.clone());
        if s.kind == Kind::Dir {
            plan.dirs.push(s.clone());
        }
        let Some(d) = dest_by_key.get(&key).copied() else {
            create(&mut plan, s);
            continue;
        };
        matched.insert(&d.rel);
        if name_key(d.rel.file_name(), false) != name_key(s.rel.file_name(), false) {
            let from = RelPath::child(s.rel.parent().as_ref(), d.rel.file_name())
                .expect("dest name is valid");
            plan.renames.push((from, s.rel.clone()));
            renamed.insert(d.rel.as_str().to_string(), s.rel.as_str().to_string());
        }
        match (s.kind, d.kind) {
            (Kind::Dir, Kind::Dir) => {}
            (Kind::File, Kind::File) => {
                if s.size == d.size && s.mtime == d.mtime {
                    plan.totals.unchanged_files += 1;
                    plan.totals.unchanged_bytes += s.size;
                } else {
                    plan.totals.replaced_bytes += d.size;
                    create(&mut plan, s);
                }
            }
            (Kind::Symlink, Kind::Symlink) => {
                if s.link_target == d.link_target {
                    plan.totals.unchanged_files += 1;
                } else {
                    plan.clear.push(s.rel.clone());
                    create(&mut plan, s);
                }
            }
            // Renaming the copied file over a symlink replaces the link itself.
            (Kind::File, Kind::Symlink) => create(&mut plan, s),
            (_, Kind::Dir) => {
                replaced_dirs.push(&d.rel);
                plan.replace_roots.push(s.rel.clone());
                create(&mut plan, s);
            }
            (_, _) => {
                if d.kind == Kind::File {
                    plan.totals.replaced_bytes += d.size;
                }
                plan.clear.push(s.rel.clone());
                create(&mut plan, s);
            }
        }
    }

    for d in dest {
        let item = DeleteItem {
            rel: effective(&d.rel, &renamed),
            kind: d.kind,
            size: d.size,
        };
        if replaced_dirs.iter().any(|x| d.rel.starts_with(x)) {
            plan.replace_trees.push(item);
        } else if !matched.contains(&d.rel) {
            plan.deletes.push(item);
        }
    }
    deepest_first(&mut plan.deletes);
    deepest_first(&mut plan.replace_trees);
    plan.temp_cleanup = temp_files.iter().map(|t| effective(t, &renamed)).collect();
    plan.dirs.sort_by(|a, b| {
        b.rel
            .depth()
            .cmp(&a.rel.depth())
            .then_with(|| a.rel.cmp(&b.rel))
    });

    let t = &mut plan.totals;
    t.copy_files = plan.copies.len() as u64;
    t.copy_bytes = plan.copies.iter().map(|c| c.size).sum();
    t.delete_entries = (plan.deletes.len() + plan.replace_trees.len()) as u64;
    t.delete_bytes = plan
        .deletes
        .iter()
        .chain(&plan.replace_trees)
        .filter(|x| x.kind == Kind::File)
        .map(|x| x.size)
        .sum();
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(p: &str) -> RelPath {
        RelPath::new(p).unwrap()
    }
    fn f(rel: &str, size: u64, mtime: i64) -> Entry {
        Entry {
            rel: r(rel),
            kind: Kind::File,
            size,
            mtime,
            mode: 0o644,
            link_target: None,
        }
    }
    fn d(rel: &str) -> Entry {
        Entry {
            rel: r(rel),
            kind: Kind::Dir,
            size: 0,
            mtime: 0,
            mode: 0o755,
            link_target: None,
        }
    }
    fn l(rel: &str, target: &str) -> Entry {
        Entry {
            rel: r(rel),
            kind: Kind::Symlink,
            size: 0,
            mtime: 0,
            mode: 0o755,
            link_target: Some(target.into()),
        }
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
        assert!(p.deletes.is_empty() && p.renames.is_empty() && p.clear.is_empty());
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
        assert_eq!(newer.totals.replaced_bytes, 3);
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
    fn file_replaced_by_directory() {
        let p = build(&[d("x"), f("x/c", 1, 1)], &[f("x", 9, 1)], &[], true);
        assert_eq!(names(&p.clear), ["x"]);
        assert_eq!(names(&p.mkdirs), ["x"]);
        assert_eq!(names(p.copies.iter().map(|c| &c.rel)), ["x/c"]);
        assert!(p.replace_trees.is_empty() && p.deletes.is_empty());
        assert_eq!(p.totals.replaced_bytes, 9);
    }

    #[test]
    fn directory_replaced_by_file_is_a_gated_tree_removal() {
        let p = build(&[f("x", 1, 1)], &[d("x"), f("x/c", 1, 1)], &[], true);
        assert_eq!(names(p.replace_trees.iter().map(|x| &x.rel)), ["x/c", "x"]);
        assert_eq!(names(&p.replace_roots), ["x"]);
        assert_eq!(names(p.copies.iter().map(|c| &c.rel)), ["x"]);
        assert!(p.deletes.is_empty());
        assert_eq!(p.totals.delete_entries, 2);
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
    fn names_colliding_on_a_case_insensitive_destination_are_reported() {
        let p = build(&[f("A.txt", 1, 1), f("a.txt", 2, 1)], &[], &[], true);
        assert_eq!(names(p.copies.iter().map(|c| &c.rel)), ["A.txt"]);
        assert_eq!(p.collisions, vec![(r("a.txt"), r("A.txt"))]);
    }

    #[test]
    fn symlinks_compare_by_target() {
        let changed = build(&[l("l", "new")], &[l("l", "old")], &[], true);
        assert_eq!(names(&changed.clear), ["l"]);
        assert_eq!(names(changed.links.iter().map(|x| &x.rel)), ["l"]);
        let same = build(&[l("l", "t")], &[l("l", "t")], &[], true);
        assert!(same.links.is_empty() && same.clear.is_empty());
    }

    #[test]
    fn file_over_symlink_is_a_plain_copy() {
        let p = build(&[f("x", 1, 1)], &[l("x", "t")], &[], true);
        assert_eq!(names(p.copies.iter().map(|c| &c.rel)), ["x"]);
        assert!(p.clear.is_empty() && p.deletes.is_empty());
    }

    #[test]
    fn dirs_are_listed_deepest_first_for_finalize() {
        let p = build(&[d("a"), d("a/b")], &[], &[], true);
        assert_eq!(names(p.dirs.iter().map(|e| &e.rel)), ["a/b", "a"]);
    }
}
