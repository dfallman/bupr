//! The only code that modifies a destination (spec §3.1). Every operation goes
//! through a cap-std `Dir` opened on the destination root, which refuses to
//! resolve `..`, absolute paths or symlinks leading outside it. Attributes are
//! applied through descriptors obtained from that handle, never by path.
#![allow(clippy::disallowed_methods)]

use std::fs::{FileTimes, Permissions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cap_fs_ext::{DirExt, SystemTimeSpec};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions};
use serde::{Deserialize, Serialize};
use xattr::FileExt;

use crate::relpath::RelPath;
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
    fn remove_nondir(&self, rel: &RelPath) -> io::Result<()>;
    fn remove_dir(&self, rel: &RelPath) -> io::Result<()>;
    fn rename(&self, from: &RelPath, to: &RelPath) -> io::Result<()>;
    fn symlink(&self, target: &Path, rel: &RelPath, mtime: i64) -> io::Result<()>;
    fn copy_file(
        &self,
        src: &mut std::fs::File,
        rel: &RelPath,
        on_progress: &mut dyn FnMut(u64),
        cancel: &AtomicBool,
    ) -> io::Result<CopyReport>;
    fn make_writable(&self, rel: Option<&RelPath>) -> io::Result<()>;
    fn set_dir_attrs(&self, rel: &RelPath, mode: u32, mtime: i64) -> io::Result<()>;
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

/// Read `src` to the end in CHUNK-sized pieces, optionally writing to `out`.
fn pump(
    src: &mut std::fs::File,
    mut out: Option<&mut std::fs::File>,
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
        if let Some(o) = out.as_deref_mut() {
            o.write_all(&buf[..n])?;
        }
        on_progress(n as u64);
    }
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_rel(rel: &RelPath) -> io::Result<RelPath> {
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

fn copy_xattrs(src: &std::fs::File, dst: &std::fs::File, warnings: &mut Vec<String>) {
    let names = match src.list_xattr() {
        Ok(n) => n,
        Err(e) => {
            warnings.push(format!("cannot list extended attributes: {e}"));
            return;
        }
    };
    for name in names {
        match src.get_xattr(&name) {
            Ok(Some(value)) => {
                if let Err(e) = dst.set_xattr(&name, &value) {
                    warnings.push(format!(
                        "extended attribute {}: {e}",
                        name.to_string_lossy()
                    ));
                }
            }
            Ok(None) => {}
            Err(e) => warnings.push(format!(
                "extended attribute {}: {e}",
                name.to_string_lossy()
            )),
        }
    }
}

pub struct Dest {
    root: Dir,
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
        Ok(Dest {
            root: Dir::open_ambient_dir(root_path, ambient_authority())?,
        })
    }

    fn dir_handle(&self, rel: Option<&RelPath>) -> io::Result<Dir> {
        match rel {
            None => self.root.try_clone(),
            Some(r) => self.root.open_dir_nofollow(r.as_path()),
        }
    }
}

impl DestOps for Dest {
    fn write_marker(&self, m: &Marker) -> io::Result<()> {
        let json = serde_json::to_vec_pretty(m)?;
        let tmp = format!("{TMP_PREFIX}marker");
        self.root.write(&tmp, json)?;
        self.root.rename(&tmp, &self.root, MARKER_NAME)
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

    fn remove_nondir(&self, rel: &RelPath) -> io::Result<()> {
        self.root.remove_file_or_symlink(rel.as_path())
    }

    fn remove_dir(&self, rel: &RelPath) -> io::Result<()> {
        self.root.remove_dir(rel.as_path())
    }

    fn rename(&self, from: &RelPath, to: &RelPath) -> io::Result<()> {
        self.root.rename(from.as_path(), &self.root, to.as_path())
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
        // Metadata comes from the open descriptor *before* reading, so a file
        // modified mid-copy gets a newer mtime than recorded and is recopied next run.
        let meta = src.metadata()?;
        let tmp = temp_rel(rel)?;
        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        let mut out = self.root.open_with(tmp.as_path(), &opts)?.into_std();
        let result = (|| {
            pump(src, Some(&mut out), on_progress, cancel)?;
            let mut report = CopyReport::default();
            copy_xattrs(src, &out, &mut report.warnings);
            out.set_permissions(Permissions::from_mode(meta.mode() & 0o7777))?;
            out.set_times(
                FileTimes::new()
                    .set_modified(meta.modified()?)
                    .set_accessed(meta.accessed()?),
            )?;
            Ok(report)
        })();
        drop(out);
        let result = result.and_then(|report| {
            self.root.rename(tmp.as_path(), &self.root, rel.as_path())?;
            Ok(report)
        });
        if result.is_err() {
            let _ = self.root.remove_file(tmp.as_path());
        }
        result
    }

    fn make_writable(&self, rel: Option<&RelPath>) -> io::Result<()> {
        let f = self.dir_handle(rel)?.into_std_file();
        let mode = f.metadata()?.mode() & 0o7777;
        if mode & 0o700 != 0o700 {
            f.set_permissions(Permissions::from_mode(mode | 0o700))?;
        }
        Ok(())
    }

    fn set_dir_attrs(&self, rel: &RelPath, mode: u32, mtime: i64) -> io::Result<()> {
        let f = self.dir_handle(Some(rel))?.into_std_file();
        f.set_times(FileTimes::new().set_modified(to_system_time(mtime)))?;
        f.set_permissions(Permissions::from_mode(mode))
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
    fn remove_nondir(&self, _: &RelPath) -> io::Result<()> {
        Ok(())
    }
    fn remove_dir(&self, _: &RelPath) -> io::Result<()> {
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
        pump(src, None, on_progress, cancel)?;
        Ok(CopyReport::default())
    }
    fn make_writable(&self, _: Option<&RelPath>) -> io::Result<()> {
        Ok(())
    }
    fn set_dir_attrs(&self, _: &RelPath, _: u32, _: i64) -> io::Result<()> {
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
        let (_t, dst, src) = setup();
        tu::write(&src, "f.txt", b"hello");
        let sf = src.join("f.txt");
        tu::set_xattr(&sf, "com.example.tag", b"v");
        tu::chmod(&sf, 0o640);
        tu::set_mtime(&sf, 1_500_000_000);
        let d = Dest::open(&dst).unwrap();
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
        let d = Dest::open(&dst).unwrap();
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
        d.remove_nondir(&r("to-src")).unwrap();
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
        assert!(d.remove_nondir(&r("esc/victim.txt")).is_err());
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
        d.make_writable(Some(&r("sub"))).unwrap();
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
        sim.remove_dir(&r("a")).unwrap();
        assert!(
            !dst.exists(),
            "simulate must not even create the destination"
        );
    }
}
