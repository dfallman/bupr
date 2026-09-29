//! The only code that modifies a destination (spec §3.1). Every operation goes
//! through a cap-std `Dir` opened on the destination root, which refuses to
//! resolve `..`, absolute paths or symlinks leading outside it. Attributes are
//! applied through descriptors obtained from that handle, never by path.
//!
//! It also opens source files, because refusing to follow a symlink there
//! needs the same `OpenOptions` the write ban covers; that handle only reads.
#![allow(clippy::disallowed_methods)]

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::ffi::{CString, c_char, c_int, c_void};
use std::fs::{FileTimes, Permissions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt, SystemTimeSpec};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions, OpenOptionsExt};
use serde::{Deserialize, Serialize};
use xattr::FileExt;

use crate::relpath::RelPath;
use crate::scan::{Kind, is_volatile_xattr};
use crate::{MARKER_NAME, TMP_PREFIX};

pub(crate) const CHUNK: usize = 1 << 20;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Marker {
    pub preset: String,
    pub source: PathBuf,
    pub created: String,
}

#[derive(Debug, Default)]
pub struct CopyReport {
    pub warnings: Vec<String>,
}

/// Everything the engine may do to a destination. `Dest` performs it;
/// `SimulatedDest` only reads sources (spec §6.4).
pub trait DestOps {
    fn write_marker(&self, m: &Marker) -> io::Result<()>;
    fn mkdir(&self, rel: &RelPath) -> io::Result<()>;
    /// Removes `rel` only while it is still the entry the scan saw (inode
    /// `ino`), checked and removed through its parent's handle. Returns
    /// whether it was removed.
    fn remove_if(&self, rel: &RelPath, kind: Kind, ino: u64) -> io::Result<bool>;
    /// Renames `from` over its sibling `to` only while `to` is still inode
    /// `ino`. Returns whether it did.
    fn replace(&self, from: &RelPath, to: &RelPath, ino: u64) -> io::Result<bool>;
    /// Renames `rel` to a fresh `.bupr-tmp-*` sibling only while it is still
    /// inode `ino`, and returns that name (`None` when it changed).
    fn set_aside(&self, rel: &RelPath, ino: u64) -> io::Result<Option<RelPath>>;
    /// Removes a leftover `.bupr-tmp-*` entry and anything inside it.
    fn remove_temp(&self, rel: &RelPath) -> io::Result<()>;
    fn rename(&self, from: &RelPath, to: &RelPath) -> io::Result<()>;
    fn symlink(&self, target: &Path, rel: &RelPath, mtime: i64) -> io::Result<()>;
    fn copy_file(
        &self,
        src: &mut std::fs::File,
        rel: &RelPath,
        on_progress: &mut dyn FnMut(u64),
        cancel: &AtomicBool,
    ) -> io::Result<CopyReport>;
    /// Adds owner rwx to a directory. Returns its previous mode if it changed.
    fn make_writable(&self, rel: Option<&RelPath>) -> io::Result<Option<u32>>;
    fn set_mode(&self, rel: Option<&RelPath>, mode: u32) -> io::Result<()>;
    fn set_dir_attrs(&self, rel: &RelPath, mode: u32, mtime: i64) -> io::Result<()>;
    /// Makes everything written so far durable (AUD-M10).
    fn sync(&self) -> io::Result<()>;
}

pub fn to_system_time(secs: i64) -> SystemTime {
    if secs >= 0 {
        UNIX_EPOCH + Duration::from_secs(secs as u64)
    } else {
        UNIX_EPOCH - Duration::from_secs(secs.unsigned_abs())
    }
}

fn interrupted() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "interrupted")
}

fn changed_since_scan() -> io::Error {
    io::Error::other("changed since the scan; not copied")
}

/// Read `src` to the end in CHUNK-sized pieces (simulation).
fn pump(
    src: &mut std::fs::File,
    on_progress: &mut dyn FnMut(u64),
    cancel: &AtomicBool,
) -> io::Result<()> {
    let mut buf = vec![0u8; CHUNK];
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(interrupted());
        }
        let n = match src.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        on_progress(n as u64);
    }
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A fresh `.bupr-tmp-*` sibling of `rel`. Leftovers are removed by the next run.
pub fn temp_rel(rel: &RelPath) -> io::Result<RelPath> {
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut name = format!(
        "{TMP_PREFIX}{:x}-{n:x}-{}",
        std::process::id(),
        rel.file_name()
    );
    if name.len() > 255 {
        let mut cut = 255;
        while !name.is_char_boundary(cut) {
            cut -= 1;
        }
        name.truncate(cut);
    }
    RelPath::child(rel.parent().as_ref(), &name)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))
}

fn cstr(s: &str) -> io::Result<CString> {
    CString::new(s).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))
}

/// Orders the file's data before the rename that publishes it, without the
/// cost of a full flush per file; `sync` flushes the drive once at the end.
fn barrier(f: &std::fs::File) -> io::Result<()> {
    if unsafe { libc::fcntl(f.as_raw_fd(), libc::F_BARRIERFSYNC) } == 0 {
        return Ok(());
    }
    f.sync_data()
}

struct Progress<'a> {
    on_progress: &'a mut dyn FnMut(u64),
    cancel: &'a AtomicBool,
    reported: u64,
    /// A failed read or write of the data. copyfile retries it for as long
    /// as the callback says continue, so the callback stops the copy instead.
    error: Option<io::Error>,
}

extern "C" fn copy_status(
    what: c_int,
    stage: c_int,
    state: libc::copyfile_state_t,
    _src: *const c_char,
    _dst: *const c_char,
    ctx: *mut c_void,
) -> c_int {
    // SAFETY: `ctx` is the `Progress` that `kernel_copy` keeps alive for the call.
    let p = unsafe { &mut *(ctx as *mut Progress) };
    if p.cancel.load(Ordering::Relaxed) {
        return libc::COPYFILE_QUIT;
    }
    // Attribute errors are left to `missing_xattrs`; data errors end the copy.
    if what == libc::COPYFILE_COPY_DATA && stage == libc::COPYFILE_ERR {
        p.error = Some(io::Error::last_os_error());
        return libc::COPYFILE_QUIT;
    }
    if what == libc::COPYFILE_COPY_DATA && stage == libc::COPYFILE_PROGRESS {
        let mut copied: libc::off_t = 0;
        let ok = unsafe {
            libc::copyfile_state_get(
                state,
                libc::COPYFILE_STATE_COPIED as u32,
                &mut copied as *mut libc::off_t as *mut c_void,
            )
        } == 0;
        if ok && copied as u64 > p.reported {
            (p.on_progress)(copied as u64 - p.reported);
            p.reported = copied as u64;
        }
    }
    libc::COPYFILE_CONTINUE
}

/// Data and extended attributes through `fcopyfile(3)`: holes stay holes and
/// a filesystem-compressed file stays compressed (AUD-H5).
fn kernel_copy(
    src: &std::fs::File,
    out: &std::fs::File,
    size: u64,
    on_progress: &mut dyn FnMut(u64),
    cancel: &AtomicBool,
) -> io::Result<()> {
    let mut p = Progress {
        on_progress,
        cancel,
        reported: 0,
        error: None,
    };
    let rc = unsafe {
        let state = libc::copyfile_state_alloc();
        if state.is_null() {
            return Err(io::Error::last_os_error());
        }
        let cb: extern "C" fn(
            c_int,
            c_int,
            libc::copyfile_state_t,
            *const c_char,
            *const c_char,
            *mut c_void,
        ) -> c_int = copy_status;
        libc::copyfile_state_set(
            state,
            libc::COPYFILE_STATE_STATUS_CB as u32,
            cb as *const c_void,
        );
        libc::copyfile_state_set(
            state,
            libc::COPYFILE_STATE_STATUS_CTX as u32,
            &mut p as *mut Progress as *const c_void,
        );
        let rc = libc::fcopyfile(
            src.as_raw_fd(),
            out.as_raw_fd(),
            state,
            libc::COPYFILE_DATA | libc::COPYFILE_XATTR | libc::COPYFILE_DATA_SPARSE,
        );
        let err = io::Error::last_os_error();
        libc::copyfile_state_free(state);
        if rc == 0 { Ok(()) } else { Err(err) }
    };
    if cancel.load(Ordering::Relaxed) {
        return Err(interrupted());
    }
    if let Some(e) = p.error {
        return Err(e);
    }
    rc?;
    // Compressed data moves as attributes and holes are skipped, so not
    // every byte shows up in the callback.
    if size > p.reported {
        (p.on_progress)(size - p.reported);
    }
    Ok(())
}

/// Warn about attributes the kernel copy could not set on the destination.
fn missing_xattrs(src: &std::fs::File, dst: &std::fs::File, warnings: &mut Vec<String>) {
    let Ok(names) = src.list_xattr() else {
        return;
    };
    let have: BTreeSet<_> = dst.list_xattr().map(|n| n.collect()).unwrap_or_default();
    for n in names {
        if !is_volatile_xattr(&n) && !have.contains(&n) {
            warnings.push(format!(
                "extended attribute {} was not copied",
                n.to_string_lossy()
            ));
        }
    }
}

pub struct Dest {
    root: Dir,
    dev: u64,
    clone: bool,
    /// Directories that received a rename, synced by `sync`.
    touched: RefCell<BTreeSet<Option<RelPath>>>,
}

impl Dest {
    /// Opens the destination root, creating it and any missing ancestors.
    /// The caller must have passed preflight for `root_path` (spec §3.5).
    pub fn open(root_path: &Path) -> io::Result<Dest> {
        if !root_path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "destination must be absolute",
            ));
        }
        Dir::create_ambient_dir_all(root_path, ambient_authority())?;
        let root = Dir::open_ambient_dir(root_path, ambient_authority())?;
        let dev = cap_std::fs::MetadataExt::dev(&root.dir_metadata()?);
        Ok(Dest {
            root,
            dev,
            clone: true,
            touched: RefCell::default(),
        })
    }

    fn dir_handle(&self, rel: Option<&RelPath>) -> io::Result<Dir> {
        match rel {
            None => self.root.try_clone(),
            Some(r) => self.root.open_dir_nofollow(r.as_path()),
        }
    }

    fn touch(&self, rel: &RelPath) {
        self.touched.borrow_mut().insert(rel.parent());
    }

    /// Same-volume copies share blocks through `fclonefileat(2)`.
    fn try_clone(&self, src: &std::fs::File, tmp: &RelPath) -> bool {
        const CLONE_NOOWNERCOPY: u32 = 0x0002;
        let (Ok(dir), Ok(name)) = (
            self.dir_handle(tmp.parent().as_ref()),
            cstr(tmp.file_name()),
        ) else {
            return false;
        };
        unsafe {
            libc::fclonefileat(
                src.as_raw_fd(),
                dir.as_raw_fd(),
                name.as_ptr(),
                CLONE_NOOWNERCOPY,
            ) == 0
        }
    }

    #[cfg(test)]
    fn without_clone(mut self) -> Dest {
        self.clone = false;
        self
    }
}

impl DestOps for Dest {
    fn write_marker(&self, m: &Marker) -> io::Result<()> {
        let json = serde_json::to_vec_pretty(m)?;
        let tmp = format!("{TMP_PREFIX}marker");
        let mut opts = OpenOptions::new();
        opts.write(true)
            .create(true)
            .truncate(true)
            .follow(FollowSymlinks::No);
        let mut f = self.root.open_with(&tmp, &opts)?.into_std();
        f.write_all(&json)?;
        barrier(&f)?;
        drop(f);
        self.root.rename(&tmp, &self.root, MARKER_NAME)?;
        self.touched.borrow_mut().insert(None);
        Ok(())
    }

    fn mkdir(&self, rel: &RelPath) -> io::Result<()> {
        match self.root.create_dir(rel.as_path()) {
            Ok(()) => Ok(()),
            Err(e)
                if e.kind() == io::ErrorKind::AlreadyExists
                    && self
                        .root
                        .symlink_metadata(rel.as_path())
                        .is_ok_and(|m| m.is_dir()) =>
            {
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    fn remove_if(&self, rel: &RelPath, kind: Kind, ino: u64) -> io::Result<bool> {
        let dir = self.dir_handle(rel.parent().as_ref())?;
        let name = rel.file_name();
        if cap_std::fs::MetadataExt::ino(&dir.symlink_metadata(name)?) != ino {
            return Ok(false);
        }
        if kind == Kind::Dir {
            dir.remove_dir(name)?;
        } else {
            dir.remove_file_or_symlink(name)?;
        }
        Ok(true)
    }

    fn replace(&self, from: &RelPath, to: &RelPath, ino: u64) -> io::Result<bool> {
        if from.parent() != to.parent() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "replace needs two names in one folder",
            ));
        }
        let dir = self.dir_handle(to.parent().as_ref())?;
        if cap_std::fs::MetadataExt::ino(&dir.symlink_metadata(to.file_name())?) != ino {
            return Ok(false);
        }
        dir.rename(from.file_name(), &dir, to.file_name())?;
        self.touch(to);
        Ok(true)
    }

    fn set_aside(&self, rel: &RelPath, ino: u64) -> io::Result<Option<RelPath>> {
        let dir = self.dir_handle(rel.parent().as_ref())?;
        if cap_std::fs::MetadataExt::ino(&dir.symlink_metadata(rel.file_name())?) != ino {
            return Ok(None);
        }
        let aside = temp_rel(rel)?;
        dir.rename(rel.file_name(), &dir, aside.file_name())?;
        self.touch(rel);
        Ok(Some(aside))
    }

    fn remove_temp(&self, rel: &RelPath) -> io::Result<()> {
        if !rel.file_name().starts_with(TMP_PREFIX) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a temporary name",
            ));
        }
        if self.root.symlink_metadata(rel.as_path())?.is_dir() {
            self.root.remove_dir_all(rel.as_path())
        } else {
            self.root.remove_file_or_symlink(rel.as_path())
        }
    }

    fn rename(&self, from: &RelPath, to: &RelPath) -> io::Result<()> {
        self.root.rename(from.as_path(), &self.root, to.as_path())?;
        self.touch(to);
        Ok(())
    }

    fn symlink(&self, target: &Path, rel: &RelPath, mtime: i64) -> io::Result<()> {
        // symlink_contents stores `target` verbatim without resolving it.
        self.root.symlink_contents(target, rel.as_path())?;
        // A link's mtime is cosmetic (links compare by target), so ignore failures.
        let t = cap_std::time::SystemTime::from_std(to_system_time(mtime));
        let _ = self
            .root
            .set_symlink_times(rel.as_path(), None, Some(SystemTimeSpec::Absolute(t)));
        Ok(())
    }

    fn copy_file(
        &self,
        src: &mut std::fs::File,
        rel: &RelPath,
        on_progress: &mut dyn FnMut(u64),
        cancel: &AtomicBool,
    ) -> io::Result<CopyReport> {
        if cancel.load(Ordering::Relaxed) {
            return Err(interrupted());
        }
        // Metadata comes from the open descriptor *before* reading, so a file
        // modified mid-copy gets a newer mtime than recorded and is recopied next run.
        let meta = src.metadata()?;
        let tmp = temp_rel(rel)?;
        let cloned = self.clone && meta.dev() == self.dev && self.try_clone(src, &tmp);
        let result = (|| {
            let mut opts = OpenOptions::new();
            if cloned {
                on_progress(meta.len());
                // The clone carries the source's mode, which may be read-only;
                // setting attributes on one's own file needs no write access.
                opts.read(true).follow(FollowSymlinks::No);
            } else {
                opts.write(true).create_new(true);
            }
            let out = self.root.open_with(tmp.as_path(), &opts)?.into_std();
            let mut report = CopyReport::default();
            if !cloned {
                kernel_copy(src, &out, meta.len(), on_progress, cancel)?;
                missing_xattrs(src, &out, &mut report.warnings);
            }
            out.set_permissions(Permissions::from_mode(meta.mode() & 0o7777))?;
            out.set_times(
                FileTimes::new()
                    .set_modified(meta.modified()?)
                    .set_accessed(meta.accessed()?),
            )?;
            barrier(&out)?;
            Ok(report)
        })();
        let result = result.and_then(|report| {
            self.root.rename(tmp.as_path(), &self.root, rel.as_path())?;
            self.touch(rel);
            Ok(report)
        });
        if result.is_err() {
            let _ = self.root.remove_file(tmp.as_path());
        }
        result
    }

    fn make_writable(&self, rel: Option<&RelPath>) -> io::Result<Option<u32>> {
        let f = self.dir_handle(rel)?.into_std_file();
        let mode = f.metadata()?.mode() & 0o7777;
        if mode & 0o700 == 0o700 {
            return Ok(None);
        }
        f.set_permissions(Permissions::from_mode(mode | 0o700))?;
        Ok(Some(mode))
    }

    fn set_mode(&self, rel: Option<&RelPath>, mode: u32) -> io::Result<()> {
        let f = self.dir_handle(rel)?.into_std_file();
        f.set_permissions(Permissions::from_mode(mode))
    }

    fn set_dir_attrs(&self, rel: &RelPath, mode: u32, mtime: i64) -> io::Result<()> {
        let f = self.dir_handle(Some(rel))?.into_std_file();
        f.set_times(FileTimes::new().set_modified(to_system_time(mtime)))?;
        f.set_permissions(Permissions::from_mode(mode))
    }

    fn sync(&self) -> io::Result<()> {
        for d in self.touched.take() {
            // A folder that is gone since has nothing left to sync.
            if let Ok(dir) = self.dir_handle(d.as_ref()) {
                unsafe { libc::fsync(dir.as_raw_fd()) };
            }
        }
        // One full flush of the drive's cache for the whole run.
        if unsafe { libc::fcntl(self.root.as_raw_fd(), libc::F_FULLFSYNC) } == 0 {
            return Ok(());
        }
        if unsafe { libc::fsync(self.root.as_raw_fd()) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

/// Read-only handle on the source root. Files are opened without following
/// a symlink in any component, and only if they are still the file the scan
/// recorded (AUD-H1).
pub struct Source {
    root: Dir,
    dev: u64,
}

impl Source {
    pub fn open(root_path: &Path) -> io::Result<Source> {
        let root = Dir::open_ambient_dir(root_path, ambient_authority())?;
        let dev = cap_std::fs::MetadataExt::dev(&root.dir_metadata()?);
        Ok(Source { root, dev })
    }

    pub fn open_file(&self, rel: &RelPath, ino: u64) -> io::Result<std::fs::File> {
        // A symlink where the scan saw a folder or file shows up as ELOOP
        // or ENOTDIR.
        let swapped = |e: io::Error| match e.raw_os_error() {
            Some(libc::ELOOP | libc::ENOTDIR) => changed_since_scan(),
            _ => e,
        };
        let mut dir = self.root.try_clone()?;
        if let Some(parent) = rel.parent() {
            for c in parent.components() {
                dir = dir.open_dir_nofollow(c).map_err(swapped)?;
            }
        }
        let mut opts = OpenOptions::new();
        // O_NONBLOCK: a FIFO swapped in since the scan must not hang the open.
        opts.read(true)
            .follow(FollowSymlinks::No)
            .custom_flags(libc::O_NONBLOCK);
        let f = dir
            .open_with(rel.file_name(), &opts)
            .map_err(swapped)?
            .into_std();
        let m = f.metadata()?;
        if !m.is_file() || m.ino() != ino || m.dev() != self.dev {
            return Err(changed_since_scan());
        }
        Ok(f)
    }
}

/// Performs no destination changes; `copy_file` reads the source through so
/// progress, speed and read errors are real (spec §6.4).
pub struct SimulatedDest;

impl DestOps for SimulatedDest {
    fn write_marker(&self, _: &Marker) -> io::Result<()> {
        Ok(())
    }
    fn mkdir(&self, _: &RelPath) -> io::Result<()> {
        Ok(())
    }
    fn remove_if(&self, _: &RelPath, _: Kind, _: u64) -> io::Result<bool> {
        Ok(true)
    }
    fn replace(&self, _: &RelPath, _: &RelPath, _: u64) -> io::Result<bool> {
        Ok(true)
    }
    fn set_aside(&self, rel: &RelPath, _: u64) -> io::Result<Option<RelPath>> {
        temp_rel(rel).map(Some)
    }
    fn remove_temp(&self, _: &RelPath) -> io::Result<()> {
        Ok(())
    }
    fn rename(&self, _: &RelPath, _: &RelPath) -> io::Result<()> {
        Ok(())
    }
    fn symlink(&self, _: &Path, _: &RelPath, _: i64) -> io::Result<()> {
        Ok(())
    }
    fn copy_file(
        &self,
        src: &mut std::fs::File,
        _: &RelPath,
        on_progress: &mut dyn FnMut(u64),
        cancel: &AtomicBool,
    ) -> io::Result<CopyReport> {
        pump(src, on_progress, cancel)?;
        Ok(CopyReport::default())
    }
    fn make_writable(&self, _: Option<&RelPath>) -> io::Result<Option<u32>> {
        Ok(None)
    }
    fn set_mode(&self, _: Option<&RelPath>, _: u32) -> io::Result<()> {
        Ok(())
    }
    fn set_dir_attrs(&self, _: &RelPath, _: u32, _: i64) -> io::Result<()> {
        Ok(())
    }
    fn sync(&self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil as tu;
    use std::fs;

    fn r(p: &str) -> RelPath {
        RelPath::new(p).unwrap()
    }
    fn setup() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().canonicalize().unwrap();
        tu::mkdir(&root, "src");
        (t, root.join("dst"), root.join("src"))
    }
    fn no_temp_files(dir: &Path) -> bool {
        fs::read_dir(dir).unwrap().all(|e| {
            !e.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(TMP_PREFIX)
        })
    }
    fn never() -> AtomicBool {
        AtomicBool::new(false)
    }

    #[test]
    fn copy_file_preserves_content_mode_mtime_and_xattrs() {
        for clone in [true, false] {
            copy_preserves(clone);
        }
    }

    fn copy_preserves(clone: bool) {
        let (_t, dst, src) = setup();
        tu::write(&src, "f.txt", b"hello");
        let sf = src.join("f.txt");
        tu::set_xattr(&sf, "com.example.tag", b"v");
        tu::chmod(&sf, 0o640);
        tu::set_mtime(&sf, 1_500_000_000);
        let mut d = Dest::open(&dst).unwrap();
        if !clone {
            d = d.without_clone();
        }
        d.mkdir(&r("sub")).unwrap();
        let mut total = 0;
        let report = d
            .copy_file(
                &mut fs::File::open(&sf).unwrap(),
                &r("sub/f.txt"),
                &mut |n| total += n,
                &never(),
            )
            .unwrap();
        let out = dst.join("sub/f.txt");
        assert_eq!(fs::read(&out).unwrap(), b"hello");
        let m = fs::metadata(&out).unwrap();
        assert_eq!(m.mode() & 0o7777, 0o640);
        assert_eq!(m.mtime(), 1_500_000_000);
        assert_eq!(
            xattr::get(&out, "com.example.tag").unwrap().as_deref(),
            Some(&b"v"[..])
        );
        assert_eq!(total, 5);
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        assert!(no_temp_files(&dst.join("sub")));
    }

    #[test]
    fn copy_overwrites_an_existing_file() {
        let (_t, dst, src) = setup();
        tu::write(&src, "f", b"new");
        tu::write(&dst, "f", b"old contents");
        let d = Dest::open(&dst).unwrap();
        d.copy_file(
            &mut fs::File::open(src.join("f")).unwrap(),
            &r("f"),
            &mut |_| {},
            &never(),
        )
        .unwrap();
        assert_eq!(fs::read(dst.join("f")).unwrap(), b"new");
    }

    #[test]
    fn cancel_mid_file_leaves_nothing() {
        let (_t, dst, src) = setup();
        tu::write(&src, "big", &vec![7u8; 3 * CHUNK]);
        let d = Dest::open(&dst).unwrap().without_clone();
        let cancel = AtomicBool::new(false);
        let mut chunks = 0;
        let e = d
            .copy_file(
                &mut fs::File::open(src.join("big")).unwrap(),
                &r("big"),
                &mut |_| {
                    chunks += 1;
                    cancel.store(true, Ordering::Relaxed);
                },
                &cancel,
            )
            .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::Interrupted);
        assert_eq!(chunks, 1);
        assert!(!dst.join("big").exists());
        assert!(no_temp_files(&dst));
    }

    #[test]
    fn open_creates_missing_root_and_ancestors() {
        let (_t, dst, _src) = setup();
        let deep = dst.join("a/b/c");
        Dest::open(&deep).unwrap();
        assert!(deep.is_dir());
        assert!(Dest::open(Path::new("relative/x")).is_err());
    }

    #[test]
    fn mkdir_is_idempotent_and_needs_its_parent() {
        let (_t, dst, _src) = setup();
        let d = Dest::open(&dst).unwrap();
        d.mkdir(&r("a")).unwrap();
        d.mkdir(&r("a")).unwrap();
        assert!(d.mkdir(&r("x/y")).is_err());
    }

    #[test]
    fn symlink_is_stored_verbatim_and_removal_never_follows_it() {
        let (_t, dst, src) = setup();
        tu::write(&src, "keep.txt", b"k");
        let d = Dest::open(&dst).unwrap();
        d.symlink(Path::new("/etc"), &r("abs"), 1_000).unwrap();
        d.symlink(&src, &r("to-src"), 1_000).unwrap();
        assert_eq!(
            fs::read_link(dst.join("abs")).unwrap(),
            PathBuf::from("/etc")
        );
        let ino = fs::symlink_metadata(dst.join("to-src")).unwrap().ino();
        assert!(d.remove_if(&r("to-src"), Kind::Symlink, ino).unwrap());
        assert!(!dst.join("to-src").exists());
        assert_eq!(fs::read(src.join("keep.txt")).unwrap(), b"k");
    }

    #[test]
    fn operations_cannot_escape_through_a_symlink() {
        let (_t, dst, src) = setup();
        tu::write(&src, "victim.txt", b"precious");
        tu::write(&src, "payload", b"x");
        let d = Dest::open(&dst).unwrap();
        tu::symlink(src.to_str().unwrap(), &dst, "esc");
        assert!(d.mkdir(&r("esc/new")).is_err());
        assert!(
            d.copy_file(
                &mut fs::File::open(src.join("payload")).unwrap(),
                &r("esc/victim.txt"),
                &mut |_| {},
                &never()
            )
            .is_err()
        );
        let victim = fs::metadata(src.join("victim.txt")).unwrap().ino();
        assert!(
            d.remove_if(&r("esc/victim.txt"), Kind::File, victim)
                .is_err()
        );
        assert!(d.remove_temp(&r("esc")).is_err());
        assert!(d.set_dir_attrs(&r("esc"), 0o700, 0).is_err());
        assert!(d.make_writable(Some(&r("esc"))).is_err());
        assert!(d.rename(&r("esc/victim.txt"), &r("stolen")).is_err());
        assert_eq!(fs::read(src.join("victim.txt")).unwrap(), b"precious");
        let names: Vec<_> = fs::read_dir(&src)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(
            names.len(),
            2,
            "nothing may appear in the outside directory: {names:?}"
        );
    }

    #[test]
    fn marker_is_written_atomically() {
        let (_t, dst, _src) = setup();
        let d = Dest::open(&dst).unwrap();
        let m = Marker {
            preset: "dev".into(),
            source: "/Users/me/dev".into(),
            created: "2026-09-29T17:00:00Z".into(),
        };
        d.write_marker(&m).unwrap();
        let back: Marker =
            serde_json::from_slice(&fs::read(dst.join(MARKER_NAME)).unwrap()).unwrap();
        assert_eq!(back, m);
        assert!(no_temp_files(&dst));
    }

    #[test]
    fn dir_attrs_and_make_writable() {
        let (_t, dst, _src) = setup();
        let d = Dest::open(&dst).unwrap();
        d.mkdir(&r("sub")).unwrap();
        d.set_dir_attrs(&r("sub"), 0o555, 1_400_000_000).unwrap();
        let m = fs::metadata(dst.join("sub")).unwrap();
        assert_eq!((m.mode() & 0o7777, m.mtime()), (0o555, 1_400_000_000));
        assert_eq!(d.make_writable(Some(&r("sub"))).unwrap(), Some(0o555));
        assert_eq!(d.make_writable(Some(&r("sub"))).unwrap(), None);
        assert_eq!(
            fs::metadata(dst.join("sub")).unwrap().mode() & 0o7777,
            0o755
        );
    }

    #[test]
    fn long_names_get_valid_temp_names() {
        let (_t, dst, src) = setup();
        let long = "n".repeat(250);
        tu::write(&src, &long, b"x");
        let d = Dest::open(&dst).unwrap();
        d.copy_file(
            &mut fs::File::open(src.join(&long)).unwrap(),
            &r(&long),
            &mut |_| {},
            &never(),
        )
        .unwrap();
        assert_eq!(fs::read(dst.join(&long)).unwrap(), b"x");
    }

    #[test]
    fn simulated_dest_reads_everything_but_writes_nothing() {
        let (_t, dst, src) = setup();
        tu::write(&src, "f", &vec![1u8; CHUNK + 10]);
        let sim = SimulatedDest;
        let mut total = 0;
        sim.copy_file(
            &mut fs::File::open(src.join("f")).unwrap(),
            &r("f"),
            &mut |n| total += n,
            &never(),
        )
        .unwrap();
        assert_eq!(total, (CHUNK + 10) as u64);
        sim.mkdir(&r("a")).unwrap();
        sim.write_marker(&Marker {
            preset: "p".into(),
            source: "/".into(),
            created: String::new(),
        })
        .unwrap();
        assert!(sim.remove_if(&r("a"), Kind::Dir, 0).unwrap());
        assert!(
            !dst.exists(),
            "simulate must not even create the destination"
        );
    }

    #[test]
    fn sparse_files_stay_sparse() {
        let (_t, dst, src) = setup();
        tu::sparse(&src.join("disk.img"), 256 << 20, b"tail");
        let d = Dest::open(&dst).unwrap().without_clone();
        let mut total = 0;
        d.copy_file(
            &mut fs::File::open(src.join("disk.img")).unwrap(),
            &r("disk.img"),
            &mut |n| total += n,
            &never(),
        )
        .unwrap();
        let m = fs::metadata(dst.join("disk.img")).unwrap();
        assert_eq!((m.len(), total), ((256 << 20) + 4, (256 << 20) + 4));
        assert!(
            m.blocks() * 512 < 16 << 20,
            "{} bytes allocated",
            m.blocks() * 512
        );
        assert_eq!(
            fs::read(dst.join("disk.img")).unwrap(),
            fs::read(src.join("disk.img")).unwrap()
        );
    }

    #[test]
    fn compressed_files_stay_readable_and_compressed() {
        // A filesystem-compressed file from the system volume, if there is one.
        let sys = Path::new("/usr/share/dict/web2");
        let Ok(m) = fs::symlink_metadata(sys) else {
            eprintln!("skipping: {} missing", sys.display());
            return;
        };
        if std::os::macos::fs::MetadataExt::st_flags(&m) & libc::UF_COMPRESSED == 0 {
            eprintln!("skipping: {} is not compressed", sys.display());
            return;
        }
        let (_t, dst, _src) = setup();
        let d = Dest::open(&dst).unwrap();
        let mut total = 0;
        d.copy_file(
            &mut fs::File::open(sys).unwrap(),
            &r("web2"),
            &mut |n| total += n,
            &never(),
        )
        .unwrap();
        let out = dst.join("web2");
        assert_eq!(fs::read(&out).unwrap(), fs::read(sys).unwrap());
        assert_eq!(total, m.len());
        let flags = std::os::macos::fs::MetadataExt::st_flags(&fs::metadata(&out).unwrap());
        assert_ne!(flags & libc::UF_COMPRESSED, 0);
    }

    #[test]
    fn guarded_removal_and_replace_check_the_inode() {
        let (_t, dst, _src) = setup();
        tu::write(&dst, "a", b"a");
        tu::write(&dst, "b", b"b");
        tu::mkdir(&dst, "dir");
        let ino = |p: &str| fs::symlink_metadata(dst.join(p)).unwrap().ino();
        let d = Dest::open(&dst).unwrap();
        let (a, b) = (ino("a"), ino("b"));
        assert!(!d.remove_if(&r("a"), Kind::File, b).unwrap());
        assert!(!d.replace(&r("b"), &r("a"), b).unwrap());
        assert_eq!(fs::read(dst.join("a")).unwrap(), b"a");
        assert!(d.replace(&r("b"), &r("a"), a).unwrap());
        assert_eq!(fs::read(dst.join("a")).unwrap(), b"b");
        assert!(d.remove_if(&r("dir"), Kind::Dir, ino("dir")).unwrap());
        assert!(d.replace(&r("a"), &r("dir/x"), 0).is_err());
    }

    #[test]
    fn temp_leftovers_are_removed_whole() {
        let (_t, dst, src) = setup();
        tu::write(&dst, ".bupr-tmp-1-x/deep/f", b"f");
        tu::symlink(src.to_str().unwrap(), &dst, ".bupr-tmp-2-l");
        tu::write(&src, "keep", b"k");
        tu::write(&dst, "real", b"r");
        let d = Dest::open(&dst).unwrap();
        d.remove_temp(&r(".bupr-tmp-1-x")).unwrap();
        d.remove_temp(&r(".bupr-tmp-2-l")).unwrap();
        assert!(d.remove_temp(&r("real")).is_err());
        assert_eq!(fs::read(src.join("keep")).unwrap(), b"k");
        let names: Vec<_> = fs::read_dir(&dst)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["real"]);
    }

    #[test]
    fn source_files_open_only_if_unchanged_and_never_through_links() {
        let (_t, root, src) = setup();
        tu::write(&src, "dir/f", b"f");
        tu::write(&root, "outside/f", b"secret");
        let s = Source::open(&src).unwrap();
        let ino = fs::metadata(src.join("dir/f")).unwrap().ino();
        let mut buf = String::new();
        s.open_file(&r("dir/f"), ino)
            .unwrap()
            .read_to_string(&mut buf)
            .unwrap();
        assert_eq!(buf, "f");
        let other = fs::metadata(root.join("outside/f")).unwrap().ino();
        let changed = |e: io::Error| e.to_string().contains("changed since the scan");
        assert!(changed(s.open_file(&r("dir/f"), ino + 1).unwrap_err()));
        // The file, then its folder, swapped for symlinks to the outside.
        fs::remove_file(src.join("dir/f")).unwrap();
        tu::symlink(root.join("outside/f").to_str().unwrap(), &src, "dir/f");
        assert!(changed(s.open_file(&r("dir/f"), other).unwrap_err()));
        fs::remove_file(src.join("dir/f")).unwrap();
        fs::remove_dir(src.join("dir")).unwrap();
        tu::symlink(root.join("outside").to_str().unwrap(), &src, "dir");
        assert!(changed(s.open_file(&r("dir/f"), other).unwrap_err()));
        // A FIFO swapped in fails fast instead of blocking.
        tu::mkfifo(&src.join("fifo"));
        let fifo = fs::symlink_metadata(src.join("fifo")).unwrap().ino();
        assert!(changed(s.open_file(&r("fifo"), fifo).unwrap_err()));
    }
}
