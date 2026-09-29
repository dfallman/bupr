//! Read-only safety checks and facts about source and destination
//! (spec §3.5, §3.6). Nothing here writes.

use std::ffi::{CStr, CString};
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::MARKER_NAME;
use crate::config::Preset;
use crate::dest::Marker;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Env {
    pub home: PathBuf,
    pub volumes: PathBuf,
}

impl Env {
    pub fn system() -> Env {
        let home = crate::config::home_dir();
        Env {
            home: fs::canonicalize(&home).unwrap_or(home),
            volumes: PathBuf::from("/Volumes"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PreflightError {
    #[error("source {0} does not exist or is not a folder")]
    SourceMissing(PathBuf),
    #[error("drive not mounted: {0} is not available")]
    NotMounted(PathBuf),
    #[error("destination {0} is a protected location")]
    Protected(PathBuf),
    #[error("destination {dest} overlaps the source {src}")]
    Overlap { dest: PathBuf, src: PathBuf },
    #[error("destination {0} is a volume root; use a subfolder such as {0}/backup")]
    VolumeRoot(PathBuf),
    #[error("destination {0} is on the internal disk; set allow_internal = true to permit this")]
    Internal(PathBuf),
    #[error("cannot resolve {path}: {message}")]
    Io { path: PathBuf, message: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub source: PathBuf,
    pub dest: PathBuf,
    /// Missing ancestors of `dest`, outermost first; the worker may create them.
    pub missing_ancestors: Vec<PathBuf>,
}

const PROTECTED: &[&str] = &[
    "/",
    "/System",
    "/Library",
    "/Applications",
    "/Users",
    "/private",
    "/usr",
    "/bin",
    "/sbin",
    "/etc",
    "/var",
    "/opt",
    "/Volumes",
    "/private/var",
    "/private/etc",
    "/private/tmp",
    "/tmp",
    "/cores",
    "/dev",
];

fn nearest_existing(p: &Path) -> &Path {
    let mut cur = p;
    while fs::symlink_metadata(cur).is_err() {
        match cur.parent() {
            Some(par) => cur = par,
            None => break,
        }
    }
    cur
}

/// Canonicalize the longest existing prefix of `p` and append the rest.
/// Returns the resolved path and every missing path component (outermost first).
pub fn resolve_lenient(p: &Path) -> io::Result<(PathBuf, Vec<PathBuf>)> {
    if !p.is_absolute()
        || p.components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path must be absolute without . or .. components",
        ));
    }
    let mut existing = p.to_path_buf();
    let mut rest = Vec::new();
    while fs::symlink_metadata(&existing).is_err() {
        match (existing.file_name(), existing.parent()) {
            (Some(n), Some(par)) => {
                rest.push(n.to_owned());
                existing = par.to_path_buf();
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "no existing ancestor",
                ));
            }
        }
    }
    let mut full = fs::canonicalize(&existing)?;
    let mut missing = Vec::new();
    for n in rest.iter().rev() {
        full.push(n);
        missing.push(full.clone());
    }
    Ok((full, missing))
}

pub fn is_mount_point(p: &Path) -> bool {
    match (
        fs::symlink_metadata(p),
        p.parent().and_then(|pp| fs::metadata(pp).ok()),
    ) {
        (Ok(m), Some(pm)) if m.is_dir() => m.dev() != pm.dev(),
        _ => false,
    }
}

fn check_volume(dest: &Path, env: &Env) -> Result<(), PreflightError> {
    let Ok(rest) = dest.strip_prefix(&env.volumes) else {
        return Ok(());
    };
    let mut comps = rest.components();
    let Some(first) = comps.next() else {
        return Err(PreflightError::Protected(dest.to_path_buf()));
    };
    let vol = env.volumes.join(first);
    if comps.next().is_none() {
        return Err(PreflightError::VolumeRoot(dest.to_path_buf()));
    }
    if !is_mount_point(&vol) {
        return Err(PreflightError::NotMounted(vol));
    }
    Ok(())
}

fn dev_of(p: &Path) -> Option<u64> {
    fs::metadata(p).ok().map(|m| m.dev())
}

pub fn check_paths(p: &Preset, env: &Env) -> Result<Resolved, PreflightError> {
    let source = fs::canonicalize(&p.source)
        .ok()
        .filter(|s| s.is_dir())
        .ok_or_else(|| PreflightError::SourceMissing(p.source.clone()))?;
    check_volume(&p.destination, env)?;
    let (dest, mut missing) = resolve_lenient(&p.destination).map_err(|e| PreflightError::Io {
        path: p.destination.clone(),
        message: e.to_string(),
    })?;
    check_volume(&dest, env)?;
    if PROTECTED.iter().any(|x| Path::new(x) == dest)
        || dest == env.volumes
        || env.home.starts_with(&dest)
    {
        return Err(PreflightError::Protected(dest));
    }
    if dest.starts_with(&source) || source.starts_with(&dest) {
        return Err(PreflightError::Overlap { dest, src: source });
    }
    if dest.exists() && is_mount_point(&dest) {
        return Err(PreflightError::VolumeRoot(dest));
    }
    if !p.allow_internal {
        let d = dev_of(nearest_existing(&dest));
        if d.is_some() && (d == dev_of(&env.home) || d == dev_of(Path::new("/"))) {
            return Err(PreflightError::Internal(dest));
        }
    }
    if missing.last() == Some(&dest) {
        missing.pop();
    }
    Ok(Resolved {
        source,
        dest,
        missing_ancestors: missing,
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum MarkerStatus {
    /// Destination missing or empty.
    Fresh,
    Matches,
    /// Non-empty (or not a directory) and no marker.
    Foreign,
    Mismatch {
        preset: String,
        source: PathBuf,
    },
}

impl MarkerStatus {
    pub fn needs_adoption(&self) -> bool {
        matches!(self, MarkerStatus::Foreign | MarkerStatus::Mismatch { .. })
    }
}

pub fn read_marker(dest: &Path) -> io::Result<Option<Marker>> {
    match fs::read(dest.join(MARKER_NAME)) {
        Ok(b) => serde_json::from_slice(&b)
            .map(Some)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

pub fn marker_status(dest: &Path, preset: &str, source: &Path) -> MarkerStatus {
    match fs::symlink_metadata(dest) {
        Err(_) => return MarkerStatus::Fresh,
        Ok(m) if !m.is_dir() => return MarkerStatus::Foreign,
        Ok(_) => {}
    }
    match read_marker(dest) {
        Ok(Some(m)) if m.preset == preset && m.source == source => MarkerStatus::Matches,
        Ok(Some(m)) => MarkerStatus::Mismatch {
            preset: m.preset,
            source: m.source,
        },
        Ok(None) => {
            let empty = fs::read_dir(dest)
                .map(|mut it| it.next().is_none())
                .unwrap_or(false);
            if empty {
                MarkerStatus::Fresh
            } else {
                MarkerStatus::Foreign
            }
        }
        Err(_) => MarkerStatus::Foreign,
    }
}

fn cstr(p: &Path) -> io::Result<CString> {
    CString::new(p.as_os_str().as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))
}

pub fn free_space(p: &Path) -> io::Result<u64> {
    let c = cstr(nearest_existing(p))?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(s.f_bavail as u64 * s.f_frsize)
}

/// macOS default (APFS/HFS+) is case-insensitive; unknown is treated as such.
pub fn is_case_insensitive(p: &Path) -> bool {
    let Ok(c) = cstr(nearest_existing(p)) else {
        return true;
    };
    unsafe { libc::pathconf(c.as_ptr(), libc::_PC_CASE_SENSITIVE) != 1 }
}

pub fn mount_point(p: &Path) -> Option<PathBuf> {
    let c = cstr(nearest_existing(p)).ok()?;
    let mut s: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut s) } != 0 {
        return None;
    }
    let name = unsafe { CStr::from_ptr(s.f_mntonname.as_ptr()) };
    Some(PathBuf::from(name.to_string_lossy().into_owned()))
}

pub fn is_encrypted(mount: &Path) -> Option<bool> {
    let out = Command::new("/usr/sbin/diskutil")
        .arg("info")
        .arg("-plist")
        .arg(mount)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let xml = String::from_utf8_lossy(&out.stdout);
    match (plist_bool(&xml, "FileVault"), plist_bool(&xml, "Encrypted")) {
        (Some(true), _) | (_, Some(true)) => Some(true),
        (None, None) => None,
        _ => Some(false),
    }
}

pub fn plist_bool(xml: &str, key: &str) -> Option<bool> {
    let needle = format!("<key>{key}</key>");
    let rest = xml[xml.find(&needle)? + needle.len()..].trim_start();
    if rest.starts_with("<true/>") {
        Some(true)
    } else if rest.starts_with("<false/>") {
        Some(false)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil as tu;

    struct Fx {
        _t: tempfile::TempDir,
        root: PathBuf,
        env: Env,
    }

    fn fx() -> Fx {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().canonicalize().unwrap();
        for d in ["home", "Volumes", "src", "backups"] {
            tu::mkdir(&root, d);
        }
        let env = Env {
            home: root.join("home"),
            volumes: root.join("Volumes"),
        };
        Fx { _t: t, root, env }
    }

    fn preset(fx: &Fx, dest: PathBuf) -> Preset {
        let mut p = Preset::minimal("dev", fx.root.join("src"), dest);
        p.allow_internal = true;
        p
    }

    #[test]
    fn accepts_a_normal_destination_and_lists_missing_ancestors() {
        let fx = fx();
        let res = check_paths(&preset(&fx, fx.root.join("backups/a/dev")), &fx.env).unwrap();
        assert_eq!(res.source, fx.root.join("src"));
        assert_eq!(res.dest, fx.root.join("backups/a/dev"));
        assert_eq!(res.missing_ancestors, vec![fx.root.join("backups/a")]);
    }

    #[test]
    fn source_must_exist() {
        let fx = fx();
        let mut p = preset(&fx, fx.root.join("backups/dev"));
        p.source = fx.root.join("nope");
        assert_eq!(
            check_paths(&p, &fx.env),
            Err(PreflightError::SourceMissing(fx.root.join("nope")))
        );
    }

    #[test]
    fn refuses_overlap_both_ways() {
        let fx = fx();
        let inside = check_paths(&preset(&fx, fx.root.join("src/backup")), &fx.env);
        assert!(
            matches!(inside, Err(PreflightError::Overlap { .. })),
            "{inside:?}"
        );
        tu::mkdir(&fx.root, "src/inner");
        let mut p = preset(&fx, fx.root.join("src"));
        p.source = fx.root.join("src/inner");
        assert!(matches!(
            check_paths(&p, &fx.env),
            Err(PreflightError::Overlap { .. })
        ));
    }

    #[test]
    fn refuses_protected_locations() {
        let fx = fx();
        for dest in [
            fx.env.home.clone(),
            fx.root.clone(),
            "/".into(),
            "/Users".into(),
            "/System".into(),
        ] {
            let r = check_paths(&preset(&fx, dest.clone()), &fx.env);
            assert!(
                matches!(r, Err(PreflightError::Protected(_))),
                "{dest:?}: {r:?}"
            );
        }
    }

    #[test]
    fn refuses_unmounted_volumes_and_volume_roots() {
        let fx = fx();
        let v = &fx.env.volumes;
        assert_eq!(
            check_paths(&preset(&fx, v.join("Nope/dev")), &fx.env),
            Err(PreflightError::NotMounted(v.join("Nope")))
        );
        tu::mkdir(v, "Plain");
        assert_eq!(
            check_paths(&preset(&fx, v.join("Plain/dev")), &fx.env),
            Err(PreflightError::NotMounted(v.join("Plain")))
        );
        assert_eq!(
            check_paths(&preset(&fx, v.join("Plain")), &fx.env),
            Err(PreflightError::VolumeRoot(v.join("Plain")))
        );
        assert!(matches!(
            check_paths(&preset(&fx, v.clone()), &fx.env),
            Err(PreflightError::Protected(_))
        ));
    }

    #[test]
    fn refuses_the_internal_disk_unless_allowed() {
        let fx = fx();
        let mut p = preset(&fx, fx.root.join("backups/dev"));
        p.allow_internal = false;
        assert_eq!(
            check_paths(&p, &fx.env),
            Err(PreflightError::Internal(fx.root.join("backups/dev")))
        );
    }

    #[test]
    fn dotted_destinations_are_rejected() {
        let fx = fx();
        let p = preset(&fx, fx.root.join("backups/../src/x"));
        assert!(matches!(
            check_paths(&p, &fx.env),
            Err(PreflightError::Io { .. })
        ));
    }

    #[test]
    fn marker_status_cases() {
        let fx = fx();
        let dest = fx.root.join("backups/dev");
        let src = fx.root.join("src");
        assert_eq!(marker_status(&dest, "dev", &src), MarkerStatus::Fresh);
        tu::mkdir(&fx.root, "backups/dev");
        assert_eq!(marker_status(&dest, "dev", &src), MarkerStatus::Fresh);
        tu::write(&dest, "someone-else.txt", b"x");
        assert_eq!(marker_status(&dest, "dev", &src), MarkerStatus::Foreign);
        let m = Marker {
            preset: "dev".into(),
            source: src.clone(),
            created: "t".into(),
        };
        tu::write(&dest, MARKER_NAME, &serde_json::to_vec(&m).unwrap());
        assert_eq!(marker_status(&dest, "dev", &src), MarkerStatus::Matches);
        assert_eq!(
            marker_status(&dest, "music", &src),
            MarkerStatus::Mismatch {
                preset: "dev".into(),
                source: src.clone()
            }
        );
        tu::write(&fx.root, "backups/file", b"x");
        assert_eq!(
            marker_status(&fx.root.join("backups/file"), "dev", &src),
            MarkerStatus::Foreign
        );
        assert!(MarkerStatus::Foreign.needs_adoption() && !MarkerStatus::Fresh.needs_adoption());
    }

    #[test]
    fn plist_parsing() {
        let xml = "<dict>\n\t<key>Encrypted</key>\n\t<false/>\n\t<key>FileVault</key>\n\t<true/>\n</dict>";
        assert_eq!(plist_bool(xml, "Encrypted"), Some(false));
        assert_eq!(plist_bool(xml, "FileVault"), Some(true));
        assert_eq!(plist_bool(xml, "Missing"), None);
    }

    #[test]
    fn disk_facts_work_on_missing_paths_via_nearest_ancestor() {
        let fx = fx();
        let missing = fx.root.join("backups/not/yet");
        assert!(free_space(&missing).unwrap() > 0);
        let _ = is_case_insensitive(&missing);
        assert!(mount_point(&missing).is_some());
    }
}
