#![allow(clippy::disallowed_methods)]

use std::fs;
use std::path::Path;
use std::process::Command;

use bupr::worker::{sandbox_available, sandbox_profile};

fn sandboxed(profile: &str, program: &str, args: &[&Path]) -> bool {
    Command::new("/usr/bin/sandbox-exec")
        .arg("-p")
        .arg(profile)
        .arg(program)
        .args(args)
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success()
}

fn tmp() -> (tempfile::TempDir, std::path::PathBuf) {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().canonicalize().unwrap();
    (t, p)
}

#[test]
fn profile_denies_writes_outside_the_destination() {
    if !sandbox_available() {
        eprintln!("skipping: sandbox-exec unavailable");
        return;
    }
    let (_t, root) = tmp();
    let (dest, outside) = (root.join("dest"), root.join("outside"));
    fs::create_dir_all(&dest).unwrap();
    fs::create_dir_all(&outside).unwrap();
    let profile = sandbox_profile(Some(&dest), &[]);
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
    if !sandbox_available() {
        eprintln!("skipping: sandbox-exec unavailable");
        return;
    }
    let (_t, root) = tmp();
    let media = root.join("vol/media");
    let dest = media.join("video");
    fs::create_dir_all(root.join("vol")).unwrap();
    let profile = sandbox_profile(Some(&dest), std::slice::from_ref(&media));
    assert!(sandboxed(&profile, "/bin/mkdir", &[Path::new("-p"), &dest]));
    assert!(dest.is_dir());
    assert!(!sandboxed(
        &profile,
        "/usr/bin/touch",
        &[&media.join("sibling")]
    ));
}

#[test]
fn read_only_profile_denies_everything() {
    if !sandbox_available() {
        eprintln!("skipping: sandbox-exec unavailable");
        return;
    }
    let (_t, root) = tmp();
    let profile = sandbox_profile(None, &[]);
    assert!(!sandboxed(&profile, "/usr/bin/touch", &[&root.join("x")]));
}

#[test]
fn hostile_directory_names_are_quoted_safely() {
    if !sandbox_available() {
        eprintln!("skipping: sandbox-exec unavailable");
        return;
    }
    let (_t, root) = tmp();
    let dest = root.join("we\"ird\\ (dir)");
    fs::create_dir_all(&dest).unwrap();
    let profile = sandbox_profile(Some(&dest), &[]);
    assert!(sandboxed(&profile, "/usr/bin/touch", &[&dest.join("ok")]));
    assert!(!sandboxed(&profile, "/usr/bin/touch", &[&root.join("bad")]));
}
