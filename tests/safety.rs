//! Spec §3.7: every scenario ends by asserting the sentinel tree outside the
//! destination is byte-identical.
#![allow(clippy::disallowed_methods)]

mod common;

use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};

use bupr::engine::{Decision, Event, Outcome, RunOptions};
use common::*;

fn adopt() -> Decision {
    Decision::Proceed {
        allow_deletes: true,
        adopt: true,
    }
}

#[test]
fn symlink_in_destination_pointing_outside_is_replaced_not_followed() {
    let fx = Fx::new();
    write(&fx.src, "escape/pwn.txt", b"payload");
    fs::create_dir_all(&fx.dst).unwrap();
    symlink(fx.outside.to_str().unwrap(), &fx.dst, "escape");
    let r = run_with(
        &fx,
        &fx.preset(),
        &RunOptions::default(),
        adopt(),
        &AtomicBool::new(false),
        &mut |_| {},
    );
    assert_eq!(r.stats.outcome, Outcome::Ok, "{:?}", r.stats);
    assert!(
        fs::symlink_metadata(fx.dst.join("escape"))
            .unwrap()
            .is_dir()
    );
    assert_eq!(fs::read(fx.dst.join("escape/pwn.txt")).unwrap(), b"payload");
    fx.assert_outside_untouched();
}

#[test]
fn stray_symlink_in_destination_is_deleted_as_a_link() {
    let fx = Fx::new();
    write(&fx.src, "a.txt", b"a");
    run(&fx, &fx.preset());
    symlink(fx.outside.to_str().unwrap(), &fx.dst, "stray");
    let r = run(&fx, &fx.preset());
    assert_eq!(r.stats.deleted, 1);
    assert!(fs::symlink_metadata(fx.dst.join("stray")).is_err());
    fx.assert_outside_untouched();
}

#[test]
fn source_symlinks_to_root_and_outside_are_copied_as_links() {
    let fx = Fx::new();
    symlink("/", &fx.src, "root");
    symlink(fx.outside.to_str().unwrap(), &fx.src, "out");
    let r = run(&fx, &fx.preset());
    assert_eq!(r.stats.outcome, Outcome::Ok);
    assert_eq!(
        fs::read_link(fx.dst.join("root")).unwrap().to_str(),
        Some("/")
    );
    assert_eq!(fs::read_link(fx.dst.join("out")).unwrap(), fx.outside);
    // A second run with the links present must not follow them either.
    run(&fx, &fx.preset());
    fx.assert_outside_untouched();
}

#[test]
fn directory_swapped_for_a_symlink_mid_run_cannot_redirect_writes() {
    let fx = Fx::new();
    write(&fx.src, "sub/a.txt", b"a1");
    write(&fx.src, "sub/b.txt", b"b1");
    run(&fx, &fx.preset());
    write(&fx.src, "sub/a.txt", b"a2 changed");
    write(&fx.src, "sub/b.txt", b"b2 changed");
    let (dst, outside) = (fx.dst.clone(), fx.outside.clone());
    let r = run_with(
        &fx,
        &fx.preset(),
        &RunOptions::default(),
        adopt(),
        &AtomicBool::new(false),
        &mut |e| {
            if let Event::FileStart { path, .. } = e
                && path == "sub/a.txt"
            {
                fs::remove_dir_all(dst.join("sub")).unwrap();
                std::os::unix::fs::symlink(&outside, dst.join("sub")).unwrap();
            }
        },
    );
    assert_eq!(r.stats.outcome, Outcome::Errors, "{:?}", r.stats);
    fx.assert_outside_untouched();
}

#[test]
fn destination_inside_the_source_is_refused() {
    let fx = Fx::new();
    write(&fx.src, "a.txt", b"a");
    let mut p = fx.preset();
    p.destination = fx.src.join("backup");
    let r = run(&fx, &p);
    assert_eq!(r.stats.outcome, Outcome::PreflightFailed);
    assert!(!fx.src.join("backup").exists());
    fx.assert_outside_untouched();
}

#[test]
fn destination_at_a_volume_root_is_refused() {
    let fx = Fx::new();
    write(&fx.src, "a.txt", b"a");
    fs::create_dir_all(fx.root.join("Volumes/Disk")).unwrap();
    let mut p = fx.preset();
    p.destination = fx.root.join("Volumes/Disk");
    let r = run(&fx, &p);
    assert_eq!(r.stats.outcome, Outcome::PreflightFailed);
    assert!(files(&fx.root.join("Volumes/Disk")).is_empty());
}

#[test]
fn ctrl_c_during_deletes_stops_and_touches_nothing_else() {
    let fx = Fx::new();
    write(&fx.src, "a.txt", b"a");
    run(&fx, &fx.preset());
    for n in 0..5 {
        write(&fx.dst, &format!("extra{n}"), b"x");
    }
    let cancel = AtomicBool::new(false);
    let r = run_with(
        &fx,
        &fx.preset(),
        &RunOptions::default(),
        Decision::Proceed {
            allow_deletes: true,
            adopt: false,
        },
        &cancel,
        &mut |e| {
            if matches!(e, Event::Deleted { .. }) {
                cancel.store(true, Ordering::Relaxed);
            }
        },
    );
    assert_eq!(r.stats.outcome, Outcome::Interrupted);
    assert_eq!(r.stats.deleted, 1);
    assert!(fx.dst.join("a.txt").exists());
    fx.assert_outside_untouched();
}

#[test]
fn hostile_file_names_stay_inside() {
    let fx = Fx::new();
    let names = [
        "-rf",
        "a\nb",
        "...",
        " lead",
        "ütf ö",
        "$(touch pwned)",
        "--help",
    ];
    for n in names {
        write(&fx.src, n, n.as_bytes());
    }
    let r = run(&fx, &fx.preset());
    assert_eq!(r.stats.outcome, Outcome::Ok, "{:?}", r.stats);
    for n in names {
        assert_eq!(fs::read(fx.dst.join(n)).unwrap(), n.as_bytes());
    }
    assert!(!std::path::Path::new("pwned").exists());
    fx.assert_outside_untouched();
}
