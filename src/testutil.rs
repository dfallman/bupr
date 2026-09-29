//! Fixture writers for unit tests. Compiled only under cfg(test) (see the
//! `#[cfg(test)] mod testutil;` line in lib.rs, checked by tests/lint_guard.rs),
//! so the write-API ban does not apply to the shipped binary.
#![allow(clippy::disallowed_methods, dead_code)]

use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

pub fn write(root: &Path, rel: &str, content: &[u8]) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(&p, content).unwrap();
}

pub fn mkdir(root: &Path, rel: &str) {
    fs::create_dir_all(root.join(rel)).unwrap();
}

pub fn symlink(target: &str, root: &Path, rel: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(target, p).unwrap();
}

pub fn set_mtime(path: &Path, secs: i64) {
    let f = fs::File::open(path).unwrap();
    f.set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(secs as u64)))
        .unwrap();
}

pub fn chmod(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

pub fn set_xattr(path: &Path, name: &str, value: &[u8]) {
    xattr::set(path, name, value).unwrap();
}

pub fn mkfifo(path: &Path) {
    let c = CString::new(path.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0);
}
