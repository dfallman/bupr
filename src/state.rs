//! Writes bupr's own files — its config and run history — and nothing else
//! (spec §3). Called only by the parent process, never by the worker.
#![allow(clippy::disallowed_methods)]

use std::fs::{self, File};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use crate::history::RunRecord;

fn ensure_parent(path: &Path) -> io::Result<()> {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => fs::create_dir_all(p),
        _ => Ok(()),
    }
}

fn parent(path: &Path) -> &Path {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    }
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!("{name}{suffix}"))
}

/// Exclusive `flock`, released when `f` is closed.
fn flock(f: &File) -> io::Result<()> {
    loop {
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } == 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

/// Locks the config's folder until the handle drops, so bupr processes take
/// turns rewriting the config. Leaves no lock file behind.
fn lock_config(path: &Path) -> io::Result<File> {
    ensure_parent(path)?;
    let dir = File::open(parent(path))?;
    flock(&dir)?;
    Ok(dir)
}

/// Best effort: by now the rename has happened.
fn sync_parent(path: &Path) {
    if let Ok(d) = File::open(parent(path)) {
        let _ = d.sync_all();
    }
}

/// A new owner-only file holding `text`, flushed to disk.
fn create_private(path: &Path, text: &str) -> io::Result<()> {
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(text.as_bytes())?;
    f.sync_all()
}

/// Replace via `<path>.tmp`, clearing one a crashed write left behind.
fn atomic_write(path: &Path, text: &str) -> io::Result<()> {
    let tmp = sibling(path, ".tmp");
    match fs::remove_file(&tmp) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    if let Err(e) = create_private(&tmp, text) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    fs::rename(&tmp, path)?;
    sync_parent(path);
    Ok(())
}

fn create_config(path: &Path, text: &str) -> io::Result<()> {
    create_private(path, text)?;
    sync_parent(path);
    Ok(())
}

pub fn append_history(path: &Path, rec: &RunRecord) -> io::Result<()> {
    ensure_parent(path)?;
    let mut line = serde_json::to_string(rec)?;
    line.push('\n');
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)?;
    // One write under the lock: concurrent runs never interleave a line.
    flock(&f)?;
    f.write_all(line.as_bytes())
}

/// FNV-1a: a lock file name that stays the same across bupr builds.
fn stable_hash(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ b as u64).wrapping_mul(0x0100_0000_01b3)
    })
}

/// Takes the run lock for one destination, held until the handle drops
/// (AUD-H7). The lock file lives in `dir` (bupr's state folder), keyed by the
/// destination, so taking it writes nothing to the destination. `Ok(None)`
/// when another bupr process holds it.
pub fn lock_destination(dir: &Path, key: &str) -> io::Result<Option<File>> {
    fs::create_dir_all(dir)?;
    let path = dir.join(format!("{:016x}.lock", stable_hash(key)));
    let mut f = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?;
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let e = io::Error::last_os_error();
        return match e.raw_os_error() {
            Some(libc::EWOULDBLOCK) => Ok(None),
            _ => Err(e),
        };
    }
    f.set_len(0)?;
    writeln!(f, "{key}\n{}", std::process::id())?;
    Ok(Some(f))
}

pub fn write_new_config(path: &Path, text: &str) -> io::Result<()> {
    let _lock = lock_config(path)?;
    create_config(path, text)
}

pub fn replace_config(path: &Path, text: &str) -> io::Result<()> {
    let _lock = lock_config(path)?;
    atomic_write(path, text)
}

/// Append `block` to the config as it is on disk now (not as it was when the
/// caller last read it) and save that if `validate` accepts it. Creates the
/// config if there is none.
pub fn append_config<E: From<io::Error>>(
    path: &Path,
    block: &str,
    validate: impl FnOnce(&str) -> Result<(), E>,
) -> Result<(), E> {
    let _lock = lock_config(path)?;
    let (mut text, exists) = match fs::read_to_string(path) {
        Ok(t) => (t, true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => (String::new(), false),
        Err(e) => return Err(e.into()),
    };
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(block);
    validate(&text)?;
    if exists {
        atomic_write(path, &text)?;
    } else {
        create_config(path, &text)?;
    }
    Ok(())
}

/// Copy the config to `<config>.edit` for the user's editor.
/// Also returns the config text the edit starts from, for `commit_edit`.
pub fn begin_edit(path: &Path) -> io::Result<(PathBuf, String)> {
    let edit = sibling(path, ".edit");
    let original = fs::read_to_string(path)?;
    fs::copy(path, &edit)?;
    Ok((edit, original))
}

/// Saves the edit over the config, unless the config changed since
/// `begin_edit` (another `bupr new` or `bupr edit`): then returns false and
/// leaves both files alone.
pub fn commit_edit(edit: &Path, path: &Path, original: &str) -> io::Result<bool> {
    let _lock = lock_config(path)?;
    if fs::read_to_string(path)? != original {
        return Ok(false);
    }
    File::open(edit)?.sync_all()?;
    fs::rename(edit, path)?;
    sync_parent(path);
    Ok(true)
}

pub fn discard_edit(edit: &Path) -> io::Result<()> {
    fs::remove_file(edit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Mode, RunStats};
    use std::os::unix::fs::PermissionsExt;

    fn tmp() -> (tempfile::TempDir, PathBuf) {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().canonicalize().unwrap();
        (t, p)
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn accept(_: &str) -> io::Result<()> {
        Ok(())
    }

    #[test]
    fn history_appends_one_json_line_per_run_and_skips_garbage() {
        let (_t, dir) = tmp();
        let path = dir.join("state/bupr/history.jsonl");
        let t = jiff::Timestamp::now();
        let rec = RunRecord::new("dev", t, t, false, &RunStats::new(Mode::Run));
        append_history(&path, &rec).unwrap();
        append_history(&path, &rec).unwrap();
        let mut text = fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 2);
        text.push_str("not json\n");
        fs::write(&path, text).unwrap();
        assert_eq!(crate::history::read_all(&path).len(), 2);
    }

    #[test]
    fn concurrent_history_lines_never_interleave() {
        let (_t, dir) = tmp();
        let path = dir.join("history.jsonl");
        let t = jiff::Timestamp::now();
        let mut rec = RunRecord::new("dev", t, t, false, &RunStats::new(Mode::Run));
        rec.message = Some("x".repeat(20_000));
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| {
                    for _ in 0..25 {
                        append_history(&path, &rec).unwrap();
                    }
                });
            }
        });
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 200);
        assert_eq!(crate::history::read_all(&path).len(), 200);
    }

    #[test]
    fn config_and_history_files_are_owner_only() {
        let (_t, dir) = tmp();
        let new = dir.join("new.toml");
        write_new_config(&new, "a = 1\n").unwrap();
        assert_eq!(mode(&new), 0o600);

        let replaced = dir.join("replaced.toml");
        fs::write(&replaced, "old").unwrap();
        fs::set_permissions(&replaced, fs::Permissions::from_mode(0o644)).unwrap();
        replace_config(&replaced, "new").unwrap();
        assert_eq!(mode(&replaced), 0o600);

        let appended = dir.join("appended.toml");
        append_config(&appended, "a = 1\n", accept).unwrap();
        assert_eq!(mode(&appended), 0o600);

        let history = dir.join("history.jsonl");
        let t = jiff::Timestamp::now();
        let rec = RunRecord::new("dev", t, t, false, &RunStats::new(Mode::Run));
        append_history(&history, &rec).unwrap();
        assert_eq!(mode(&history), 0o600);
    }

    #[test]
    fn new_config_is_never_overwritten() {
        let (_t, dir) = tmp();
        let path = dir.join("cfg/bupr/config.toml");
        write_new_config(&path, "a = 1\n").unwrap();
        let e = write_new_config(&path, "b = 2\n").unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(&path).unwrap(), "a = 1\n");
    }

    #[test]
    fn replace_is_atomic_and_leaves_no_temp_file() {
        let (_t, dir) = tmp();
        let path = dir.join("config.toml");
        write_new_config(&path, "old").unwrap();
        replace_config(&path, "new").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
    }

    #[test]
    fn a_crashed_writes_temp_file_is_cleared() {
        let (_t, dir) = tmp();
        let path = dir.join("config.toml");
        let stale = dir.join("config.toml.tmp");
        write_new_config(&path, "old").unwrap();
        fs::write(&stale, "half a conf").unwrap();
        fs::set_permissions(&stale, fs::Permissions::from_mode(0o644)).unwrap();
        replace_config(&path, "new").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        assert_eq!(mode(&path), 0o600);
        assert!(!stale.exists());

        fs::write(&stale, "half a conf").unwrap();
        append_config(&path, "more\n", accept).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "new\nmore\n");
        assert!(!stale.exists());
    }

    #[test]
    fn append_uses_the_config_as_it_is_now() {
        let (_t, dir) = tmp();
        let path = dir.join("cfg/config.toml");
        append_config(&path, "a = 1\n", accept).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "a = 1\n");
        // Someone else saved meanwhile, without a final newline.
        fs::write(&path, "a = 1\nb = 2").unwrap();
        let mut seen = String::new();
        append_config(&path, "\nc = 3\n", |t: &str| {
            seen = t.to_string();
            Ok::<(), io::Error>(())
        })
        .unwrap();
        assert_eq!(seen, "a = 1\nb = 2\n\nc = 3\n");
        assert_eq!(fs::read_to_string(&path).unwrap(), seen);
    }

    #[test]
    fn append_rejected_by_validator_changes_nothing() {
        let (_t, dir) = tmp();
        let path = dir.join("config.toml");
        write_new_config(&path, "a = 1\n").unwrap();
        let e = append_config(&path, "a = 2\n", |_: &str| {
            Err(io::Error::other("duplicate key"))
        })
        .unwrap_err();
        assert_eq!(e.to_string(), "duplicate key");
        assert_eq!(fs::read_to_string(&path).unwrap(), "a = 1\n");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        let missing = dir.join("missing.toml");
        assert!(append_config(&missing, "x", |_: &str| Err(io::Error::other("no"))).is_err());
        assert!(!missing.exists());
    }

    #[test]
    fn config_lock_excludes_other_writers() {
        let (_t, dir) = tmp();
        let path = dir.join("config.toml");
        let try_lock = || {
            let d = File::open(&dir).unwrap();
            unsafe { libc::flock(d.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
        };
        let held = lock_config(&path).unwrap();
        assert!(!try_lock());
        drop(held);
        assert!(try_lock());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
    }

    #[test]
    fn edit_round_trip() {
        let (_t, dir) = tmp();
        let path = dir.join("config.toml");
        write_new_config(&path, "v1").unwrap();
        let (edit, original) = begin_edit(&path).unwrap();
        assert_eq!(
            (edit.clone(), original.as_str()),
            (dir.join("config.toml.edit"), "v1")
        );
        fs::write(&edit, "v2").unwrap();
        assert!(commit_edit(&edit, &path, &original).unwrap());
        assert_eq!(fs::read_to_string(&path).unwrap(), "v2");
        assert!(!edit.exists());
        let (edit, _) = begin_edit(&path).unwrap();
        discard_edit(&edit).unwrap();
        assert!(!edit.exists());
        assert!(begin_edit(&dir.join("missing.toml")).is_err());
    }

    #[test]
    fn an_edit_never_overwrites_a_config_changed_meanwhile() {
        let (_t, dir) = tmp();
        let path = dir.join("config.toml");
        write_new_config(&path, "v1\n").unwrap();
        let (edit, original) = begin_edit(&path).unwrap();
        fs::write(&edit, "mine\n").unwrap();
        append_config(&path, "added by new\n", accept).unwrap();
        assert!(!commit_edit(&edit, &path, &original).unwrap());
        assert_eq!(fs::read_to_string(&path).unwrap(), "v1\nadded by new\n");
        assert_eq!(fs::read_to_string(&edit).unwrap(), "mine\n");
    }

    #[test]
    fn a_destination_is_locked_by_one_holder_at_a_time() {
        let (_t, dir) = tmp();
        let locks = dir.join("locks");
        let held = lock_destination(&locks, "/volumes/b/dev").unwrap();
        assert!(held.is_some());
        assert!(
            lock_destination(&locks, "/volumes/b/dev")
                .unwrap()
                .is_none()
        );
        assert!(
            lock_destination(&locks, "/volumes/b/photos")
                .unwrap()
                .is_some()
        );
        drop(held);
        assert!(
            lock_destination(&locks, "/volumes/b/dev")
                .unwrap()
                .is_some()
        );
        assert_eq!(
            mode(
                &fs::read_dir(&locks)
                    .unwrap()
                    .next()
                    .unwrap()
                    .unwrap()
                    .path()
            ),
            0o600
        );
    }
}
