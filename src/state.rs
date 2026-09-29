//! Writes bupr's own files — its config and run history — and nothing else
//! (spec §3). Called only by the parent process, never by the worker.
#![allow(clippy::disallowed_methods)]

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::history::RunRecord;

fn ensure_parent(path: &Path) -> io::Result<()> {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => fs::create_dir_all(p),
        _ => Ok(()),
    }
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!("{name}{suffix}"))
}

fn atomic_write(path: &Path, text: &str) -> io::Result<()> {
    ensure_parent(path)?;
    let tmp = sibling(path, ".tmp");
    fs::write(&tmp, text)?;
    fs::rename(&tmp, path)
}

pub fn append_history(path: &Path, rec: &RunRecord) -> io::Result<()> {
    ensure_parent(path)?;
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(f, "{}", serde_json::to_string(rec)?)
}

pub fn write_new_config(path: &Path, text: &str) -> io::Result<()> {
    ensure_parent(path)?;
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    f.write_all(text.as_bytes())
}

pub fn replace_config(path: &Path, text: &str) -> io::Result<()> {
    atomic_write(path, text)
}

/// Copy the config to `<config>.edit` for the user's editor.
pub fn begin_edit(path: &Path) -> io::Result<PathBuf> {
    let edit = sibling(path, ".edit");
    fs::copy(path, &edit)?;
    Ok(edit)
}

pub fn commit_edit(edit: &Path, path: &Path) -> io::Result<()> {
    fs::rename(edit, path)
}

pub fn discard_edit(edit: &Path) -> io::Result<()> {
    fs::remove_file(edit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Mode, RunStats};

    fn tmp() -> (tempfile::TempDir, PathBuf) {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().canonicalize().unwrap();
        (t, p)
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
    fn edit_round_trip() {
        let (_t, dir) = tmp();
        let path = dir.join("config.toml");
        write_new_config(&path, "v1").unwrap();
        let edit = begin_edit(&path).unwrap();
        assert_eq!(edit, dir.join("config.toml.edit"));
        fs::write(&edit, "v2").unwrap();
        commit_edit(&edit, &path).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "v2");
        assert!(!edit.exists());
        let edit = begin_edit(&path).unwrap();
        discard_edit(&edit).unwrap();
        assert!(!edit.exists());
        assert!(begin_edit(&dir.join("missing.toml")).is_err());
    }
}
