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
use crate::plan::{Volume, name_key};

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

/// Refused as a destination themselves; folders inside them are fine.
const PROTECTED: &[&str] = &[
    "/",
    "/Users",
    "/private",
    "/var",
    "/Volumes",
    "/private/var",
    "/private/tmp",
    "/tmp",
];

/// System folders refused as a destination together with everything inside
/// them (AUD-M7).
const PROTECTED_TREES: &[&str] = &[
    "/System",
    "/Library",
    "/Applications",
    "/usr",
    "/bin",
    "/sbin",
    "/etc",
    "/opt",
    "/cores",
    "/dev",
    "/private/etc",
    "/private/var/db",
    "/private/var/root",
];

pub fn is_protected(dest: &Path) -> bool {
    PROTECTED.iter().any(|x| Path::new(x) == dest)
        || PROTECTED_TREES.iter().any(|x| dest.starts_with(x))
}

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

/// Identity of a destination folder for comparing presets: symlinks
/// resolved, and matched the way its volume matches names (AUD-H6).
pub fn dest_key(p: &Path) -> String {
    let resolved = resolve_lenient(p).map_or_else(|_| p.to_path_buf(), |(r, _)| r);
    name_key(&resolved.to_string_lossy(), is_case_insensitive(&resolved))
}

/// Whether two destination keys name the same folder or one inside the other.
pub fn keys_overlap(a: &str, b: &str) -> bool {
    Path::new(a).starts_with(b) || Path::new(b).starts_with(a)
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
    if is_protected(&dest) || dest == env.volumes || env.home.starts_with(&dest) {
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

fn statfs(p: &Path) -> Option<libc::statfs> {
    let c = cstr(nearest_existing(p)).ok()?;
    let mut s: libc::statfs = unsafe { std::mem::zeroed() };
    (unsafe { libc::statfs(c.as_ptr(), &mut s) } == 0).then_some(s)
}

/// Filesystem type name, such as "apfs", "hfs" or "exfat".
pub fn fs_type(p: &Path) -> Option<String> {
    let s = statfs(p)?;
    let name = unsafe { CStr::from_ptr(s.f_fstypename.as_ptr()) };
    Some(name.to_string_lossy().into_owned())
}

/// What the destination volume keeps, for comparing files (AUD-M1) and
/// estimating space (AUD-H4). Unknown filesystems get the plain size and
/// whole-second comparison.
pub fn volume(p: &Path) -> Volume {
    let fs = fs_type(p).unwrap_or_default();
    let apfs = fs == "apfs";
    let native = apfs || fs == "hfs";
    Volume {
        case_insensitive: is_case_insensitive(p),
        nanos: apfs,
        modes: native,
        xattrs: native,
        sparse: apfs,
    }
}

pub fn block_size(p: &Path) -> io::Result<u64> {
    let c = cstr(nearest_existing(p))?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(s.f_frsize)
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

/// Whether the volume at `mount` is encrypted, per `diskutil`. `None` when
/// that cannot be told; callers treat it as not known to be encrypted.
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
    encryption_from_plist(&out.stdout)
}

/// Reads the top-level `FileVault` and `Encrypted` keys of `diskutil info -plist`.
pub fn encryption_from_plist(xml: &[u8]) -> Option<bool> {
    let value = plist::Value::from_reader_xml(xml).ok()?;
    let dict = value.as_dictionary()?;
    let key = |k: &str| dict.get(k).and_then(plist::Value::as_boolean);
    match (key("FileVault"), key("Encrypted")) {
        (Some(true), _) | (_, Some(true)) => Some(true),
        (None, None) => None,
        _ => Some(false),
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
            "/usr/local/backup".into(),
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

    fn plist(body: &str) -> Vec<u8> {
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\">\n<dict>{body}</dict>\n</plist>\n"
        )
        .into_bytes()
    }

    #[test]
    fn encryption_comes_from_the_top_level_keys_only() {
        let yes = plist("<key>Encrypted</key><false/><key>FileVault</key><true/>");
        assert_eq!(encryption_from_plist(&yes), Some(true));
        let no = plist("<key>Encrypted</key><false/><key>FileVault</key><false/>");
        assert_eq!(encryption_from_plist(&no), Some(false));
        // A nested dictionary or a string that looks like the key does not count.
        let decoy = plist(
            "<key>VolumeName</key><string>&lt;key&gt;Encrypted&lt;/key&gt;&lt;true/&gt;</string>\
             <key>APFS</key><dict><key>Encrypted</key><true/></dict>",
        );
        assert_eq!(encryption_from_plist(&decoy), None);
        assert_eq!(encryption_from_plist(b"not a plist"), None);
    }

    #[test]
    fn system_folders_are_protected_with_everything_inside() {
        for p in [
            "/usr/local",
            "/System/Library",
            "/Library/Application Support",
            "/opt/homebrew",
            "/private/etc/x",
            "/Users",
            "/Volumes",
        ] {
            assert!(is_protected(Path::new(p)), "{p}");
        }
        for p in [
            "/Users/me/Backups",
            "/Volumes/Backup/dev",
            "/private/var/folders/x",
            "/private/tmp/x",
        ] {
            assert!(!is_protected(Path::new(p)), "{p}");
        }
    }

    #[test]
    fn volume_facts_follow_the_filesystem() {
        let fx = fx();
        let v = volume(&fx.root);
        if fs_type(&fx.root).as_deref() == Some("apfs") {
            assert!(v.nanos && v.modes && v.xattrs && v.sparse);
        }
        assert!(block_size(&fx.root).unwrap() >= 512);
    }

    #[test]
    fn disk_facts_work_on_missing_paths_via_nearest_ancestor() {
        let fx = fx();
        let missing = fx.root.join("backups/not/yet");
        assert!(free_space(&missing).unwrap() > 0);
        let _ = is_case_insensitive(&missing);
        assert!(mount_point(&missing).is_some());
    }

    #[test]
    fn destination_keys_see_through_symlinks_and_case() {
        let fx = fx();
        tu::symlink(fx.root.join("backups").to_str().unwrap(), &fx.root, "link");
        let a = dest_key(&fx.root.join("backups/dev"));
        assert!(keys_overlap(&a, &dest_key(&fx.root.join("link/dev"))));
        assert!(keys_overlap(&a, &dest_key(&fx.root.join("link/dev/sub"))));
        assert!(!keys_overlap(
            &a,
            &dest_key(&fx.root.join("backups/devices"))
        ));
        if is_case_insensitive(&fx.root) {
            assert!(keys_overlap(&a, &dest_key(&fx.root.join("BACKUPS/Dev"))));
        }
    }
}
