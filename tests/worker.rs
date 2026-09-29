#![allow(clippy::disallowed_methods)]

use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use bupr::worker::{sandbox_available, sandbox_profile};

fn run(profile: Option<&str>, program: &str, args: &[&Path]) -> bool {
    let mut cmd = match profile {
        Some(p) => {
            let mut c = Command::new("/usr/bin/sandbox-exec");
            c.arg("-p").arg(p).arg(program);
            c
        }
        None => Command::new(program),
    };
    cmd.args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success()
}

fn sandboxed(profile: &str, program: &str, args: &[&Path]) -> bool {
    run(Some(profile), program, args)
}

fn tmp() -> (tempfile::TempDir, PathBuf) {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().canonicalize().unwrap();
    (t, p)
}

fn profile(src: &Path, dest: &Path, missing: &[PathBuf], write: bool) -> String {
    sandbox_profile(src, dest, missing, write).unwrap()
}

macro_rules! require_sandbox {
    () => {
        if !sandbox_available() {
            eprintln!("skipping: sandbox-exec unavailable");
            return;
        }
    };
}

#[test]
fn profile_denies_writes_outside_the_destination() {
    require_sandbox!();
    let (_t, root) = tmp();
    let (dest, outside) = (root.join("dest"), root.join("outside"));
    fs::create_dir_all(&dest).unwrap();
    fs::create_dir_all(&outside).unwrap();
    let profile = profile(&root.join("src"), &dest, &[], true);
    assert!(sandboxed(&profile, "/usr/bin/touch", &[&dest.join("ok")]));
    assert!(!sandboxed(
        &profile,
        "/usr/bin/touch",
        &[&outside.join("bad")]
    ));
    assert!(!outside.join("bad").exists());
    assert!(!sandboxed(
        &profile,
        "/bin/mv",
        &[&dest.join("ok"), &outside.join("moved")]
    ));
}

#[test]
fn profile_allows_creating_only_the_missing_ancestors() {
    require_sandbox!();
    let (_t, root) = tmp();
    let media = root.join("vol/media");
    let dest = media.join("video");
    fs::create_dir_all(root.join("vol")).unwrap();
    let missing = std::slice::from_ref(&media);
    let profile = profile(&root.join("src"), &dest, missing, true);
    assert!(sandboxed(&profile, "/bin/mkdir", &[Path::new("-p"), &dest]));
    assert!(dest.is_dir());
    assert!(!sandboxed(
        &profile,
        "/usr/bin/touch",
        &[&media.join("sibling")]
    ));
    // The created ancestor is readable, its parent is not.
    assert!(sandboxed(&profile, "/bin/ls", &[&media]));
    assert!(!sandboxed(&profile, "/bin/ls", &[&root.join("vol")]));
}

#[test]
fn read_only_profile_denies_everything() {
    require_sandbox!();
    let (_t, root) = tmp();
    let dest = root.join("dest");
    fs::create_dir_all(&dest).unwrap();
    let profile = profile(&root.join("src"), &dest, &[], false);
    assert!(!sandboxed(&profile, "/usr/bin/touch", &[&root.join("x")]));
    assert!(!sandboxed(&profile, "/usr/bin/touch", &[&dest.join("x")]));
    assert!(sandboxed(&profile, "/bin/ls", &[&dest]));
}

#[test]
fn hostile_directory_names_are_quoted_safely() {
    require_sandbox!();
    let (_t, root) = tmp();
    let dest = root.join("we\"ird\\ (dir)");
    fs::create_dir_all(&dest).unwrap();
    let profile = profile(&root.join("src"), &dest, &[], true);
    assert!(sandboxed(&profile, "/usr/bin/touch", &[&dest.join("ok")]));
    assert!(!sandboxed(&profile, "/usr/bin/touch", &[&root.join("bad")]));
}

#[test]
fn paths_with_newlines_are_refused() {
    let (_t, root) = tmp();
    let evil = root.join("x\")\n(allow file-write* (subpath \"/\"))\n(\"");
    assert!(sandbox_profile(&root, &evil, &[], true).is_err());
    assert!(sandbox_profile(&evil, &root, &[], false).is_err());
    assert!(sandbox_profile(&root, &root, &[evil], true).is_err());
}

#[test]
fn reads_are_confined_to_source_and_destination() {
    require_sandbox!();
    let (_t, root) = tmp();
    for d in ["src", "dest", "outside"] {
        fs::create_dir_all(root.join(d)).unwrap();
        fs::write(root.join(d).join("f"), d).unwrap();
    }
    let (src, dest, outside) = (root.join("src"), root.join("dest"), root.join("outside"));
    for write in [true, false] {
        let profile = profile(&src, &dest, &[], write);
        assert!(sandboxed(&profile, "/bin/cat", &[&src.join("f")]));
        assert!(sandboxed(&profile, "/bin/cat", &[&dest.join("f")]));
        assert!(sandboxed(&profile, "/bin/ls", &[&src]));
        assert!(run(None, "/bin/cat", &[&outside.join("f")]));
        assert!(!sandboxed(&profile, "/bin/cat", &[&outside.join("f")]));
        assert!(!sandboxed(&profile, "/bin/ls", &[&outside]));
        assert!(sandboxed(&profile, "/usr/bin/stat", &[&outside]));
        assert!(sandboxed(&profile, "/usr/bin/stat", &[&outside.join("f")]));
    }
}

#[test]
fn symlinked_path_components_still_resolve() {
    require_sandbox!();
    let t = tempfile::tempdir().unwrap();
    let root = t.path().canonicalize().unwrap();
    // `/var/folders/...` (or `/tmp/...`): reached through a symlink.
    let via_link = t.path().to_path_buf();
    assert_ne!(via_link, root, "temp dir has no symlinked component");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/f"), "x").unwrap();
    let profile = profile(&root.join("src"), &root.join("dest"), &[], true);
    assert!(sandboxed(
        &profile,
        "/bin/realpath",
        &[&via_link.join("src")]
    ));
    assert!(sandboxed(&profile, "/bin/cat", &[&via_link.join("src/f")]));
    assert!(sandboxed(&profile, "/bin/realpath", &[Path::new("/tmp")]));
    assert!(sandboxed(
        &profile,
        "/usr/bin/touch",
        &[&via_link.join("dest")]
    ));
}

#[test]
fn network_is_denied() {
    require_sandbox!();
    let (_t, root) = tmp();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port().to_string();
    let args = [Path::new("-z"), Path::new("127.0.0.1"), Path::new(&port)];
    let profile = profile(&root, &root.join("dest"), &[], true);
    assert!(run(None, "/usr/bin/nc", &args));
    assert!(!sandboxed(&profile, "/usr/bin/nc", &args));
}

/// Run the real binary on preset `dev` (`src` → `dest`) in the fixture `root`.
fn bupr(root: &Path, src: &Path, dest: &Path, args: &[&str]) -> std::process::Output {
    for d in ["src", "home", "backup"] {
        fs::create_dir_all(root.join(d)).unwrap();
    }
    fs::write(root.join("src/a.txt"), "a").unwrap();
    let config = root.join("config.toml");
    let preset = format!("source = {src:?}\ndestination = {dest:?}\nallow_internal = true\n");
    fs::write(&config, format!("[presets.dev]\n{preset}")).unwrap();
    Command::new(env!("CARGO_BIN_EXE_bupr"))
        .env("HOME", root.join("home"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("NO_COLOR", "1")
        .env_remove("XDG_CONFIG_HOME")
        .arg("--config")
        .arg(&config)
        .arg("dev")
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn real_worker_runs_with_symlinked_preset_paths() {
    require_sandbox!();
    let t = tempfile::tempdir().unwrap();
    let (via_link, root) = (t.path().to_path_buf(), t.path().canonicalize().unwrap());
    let (src, dest) = (via_link.join("src"), via_link.join("backup/new/dev"));
    for mode in [&["--dry-run"][..], &["--simulate"], &[]] {
        let out = bupr(&root, &src, &dest, mode);
        assert!(out.status.success(), "{mode:?}: {out:?}");
    }
    assert_eq!(fs::read(root.join("backup/new/dev/a.txt")).unwrap(), b"a");
}

#[test]
fn real_run_refuses_a_destination_with_a_newline() {
    require_sandbox!();
    let (_t, root) = tmp();
    let dest = root.join("backup/x\")\n(allow default)\n(\"");
    let out = bupr(&root, &root.join("src"), &dest, &[]);
    assert!(!out.status.success());
    let text = String::from_utf8_lossy(&out.stdout) + String::from_utf8_lossy(&out.stderr);
    assert!(text.contains("cannot sandbox"), "{text}");
    assert_eq!(fs::read_dir(root.join("backup")).unwrap().count(), 0);
}
