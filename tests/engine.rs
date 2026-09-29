#![allow(clippy::disallowed_methods)]

mod common;

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::sync::atomic::{AtomicBool, Ordering};

use bupr::engine::{Decision, Event, Mode, Outcome, PlanAction, RunOptions};
use bupr::preflight::MarkerStatus;
use common::*;

fn proceed() -> Decision {
    Decision::Proceed {
        allow_deletes: true,
        adopt: false,
    }
}

#[test]
fn first_run_mirrors_and_skips_regenerable_output() {
    let fx = Fx::new();
    write(&fx.src, "p/Cargo.toml", b"[package]");
    write(&fx.src, "p/src/main.rs", b"fn main() {}");
    write(&fx.src, "p/target/big.bin", b"build output");
    write(&fx.src, "p/.env", b"KEY=1");
    write(&fx.src, "p/CLAUDE.md", b"notes");
    symlink("p/src/main.rs", &fx.src, "link");
    set_mtime(&fx.src.join("p/src/main.rs"), 1_600_000_000);
    chmod(&fx.src.join("p/CLAUDE.md"), 0o600);

    let r = run(&fx, &fx.preset());

    assert_eq!(r.stats.outcome, Outcome::Ok, "{:?}", r.stats);
    assert_eq!(
        files(&fx.dst),
        [
            ".bupr-dest",
            "link",
            "p",
            "p/.env",
            "p/CLAUDE.md",
            "p/Cargo.toml",
            "p/src",
            "p/src/main.rs"
        ]
    );
    assert_eq!(
        fs::metadata(fx.dst.join("p/src/main.rs")).unwrap().mtime(),
        1_600_000_000
    );
    assert_eq!(
        fs::metadata(fx.dst.join("p/CLAUDE.md")).unwrap().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::read_link(fx.dst.join("link")).unwrap().to_str(),
        Some("p/src/main.rs")
    );
    let s = r.summary.unwrap();
    assert_eq!(s.marker, MarkerStatus::Fresh);
    assert_eq!(s.secret_files, 1);
    assert_eq!(r.stats.copied_files, 4);
    assert!(matches!(r.events.last(), Some(Event::Done { .. })));
    fx.assert_outside_untouched();
}

#[test]
fn second_run_is_a_no_op() {
    let fx = Fx::new();
    write(&fx.src, "a.txt", b"a");
    write(&fx.src, "d/b.txt", b"b");
    run(&fx, &fx.preset());
    let r = run(&fx, &fx.preset());
    assert_eq!(r.stats.outcome, Outcome::Ok);
    assert_eq!(
        (r.stats.copied_files, r.stats.unchanged, r.stats.deleted),
        (0, 2, 0)
    );
    assert!(
        !r.events
            .iter()
            .any(|e| matches!(e, Event::FileStart { .. }))
    );
    assert_eq!(r.summary.unwrap().marker, MarkerStatus::Matches);
}

#[test]
fn changes_additions_and_removals_are_mirrored() {
    let fx = Fx::new();
    write(&fx.src, "keep.txt", b"k");
    write(&fx.src, "edit.txt", b"v1");
    write(&fx.src, "gone.txt", b"g");
    run(&fx, &fx.preset());
    write(&fx.src, "edit.txt", b"version 2");
    write(&fx.src, "new.txt", b"n");
    fs::remove_file(fx.src.join("gone.txt")).unwrap();
    let r = run(&fx, &fx.preset());
    assert_eq!((r.stats.copied_files, r.stats.deleted), (2, 1));
    assert_eq!(
        files(&fx.dst),
        [".bupr-dest", "edit.txt", "keep.txt", "new.txt"]
    );
    assert_eq!(fs::read(fx.dst.join("edit.txt")).unwrap(), b"version 2");
}

#[test]
fn newly_excluded_folders_are_cleaned_from_the_destination() {
    let fx = Fx::new();
    write(&fx.src, "big/blob", b"0123456789");
    write(&fx.src, "small.txt", b"s");
    run(&fx, &fx.preset());
    assert!(fx.dst.join("big/blob").exists());
    let mut p = fx.preset();
    p.exclude = vec!["/big/".into()];
    let r = run(&fx, &p);
    assert_eq!(r.stats.deleted, 2);
    assert_eq!(files(&fx.dst), [".bupr-dest", "small.txt"]);
}

#[test]
fn foreign_destination_is_left_alone_unless_adopted() {
    let fx = Fx::new();
    write(&fx.src, "a.txt", b"a");
    write(&fx.dst, "theirs.txt", b"not ours");
    let r = run(&fx, &fx.preset());
    assert_eq!(r.summary.unwrap().marker, MarkerStatus::Foreign);
    assert_eq!(r.stats.outcome, Outcome::Aborted);
    assert_eq!(files(&fx.dst), ["theirs.txt"]);

    let adopted = run_with(
        &fx,
        &fx.preset(),
        &RunOptions::default(),
        Decision::Proceed {
            allow_deletes: true,
            adopt: true,
        },
        &AtomicBool::new(false),
        &mut |_| {},
    );
    assert_eq!(adopted.stats.outcome, Outcome::Ok);
    assert_eq!(files(&fx.dst), [".bupr-dest", "a.txt"]);
}

#[test]
fn deletion_limit_is_reported_and_skipping_is_honoured() {
    let fx = Fx::new();
    write(&fx.src, "a.txt", b"a");
    run(&fx, &fx.preset());
    for n in ["x1", "x2", "x3"] {
        write(&fx.dst, n, b"extra");
    }
    let mut p = fx.preset();
    p.max_delete = 2;
    let r = run_with(
        &fx,
        &p,
        &RunOptions::default(),
        Decision::Proceed {
            allow_deletes: false,
            adopt: false,
        },
        &AtomicBool::new(false),
        &mut |_| {},
    );
    assert!(r.summary.unwrap().over_delete_limit);
    assert_eq!(r.stats.outcome, Outcome::DeletionsSkipped);
    assert_eq!(r.stats.outcome.exit_code(), 1);
    assert!(fx.dst.join("x1").exists());
}

#[test]
fn copy_errors_do_not_block_unrelated_deletions() {
    let fx = Fx::new();
    write(&fx.src, "ok.txt", b"ok");
    run(&fx, &fx.preset());
    write(&fx.dst, "extra.txt", b"x");
    write(&fx.src, "locked.txt", b"secret");
    chmod(&fx.src.join("locked.txt"), 0o000);
    let r = run(&fx, &fx.preset());
    assert_eq!(r.stats.outcome, Outcome::Errors);
    assert!(r.stats.errors.iter().any(|e| e.path == "locked.txt"));
    assert!(!fx.dst.join("extra.txt").exists());
    assert_eq!(r.stats.deleted, 1);
}

#[test]
fn an_unreadable_source_folder_keeps_its_backup() {
    let fx = Fx::new();
    write(&fx.src, "ok.txt", b"ok");
    write(&fx.src, "private/notes.txt", b"n");
    write(&fx.src, "gone.txt", b"g");
    run(&fx, &fx.preset());
    fs::remove_file(fx.src.join("gone.txt")).unwrap();
    chmod(&fx.src.join("private"), 0o000);
    let r = run(&fx, &fx.preset());
    chmod(&fx.src.join("private"), 0o755);
    assert_eq!(r.stats.outcome, Outcome::Errors, "{:?}", r.stats);
    assert!(r.stats.errors.iter().any(|e| e.path == "private"));
    // The folder's mode (000) is mirrored; its contents are kept.
    chmod(&fx.dst.join("private"), 0o755);
    assert_eq!(fs::read(fx.dst.join("private/notes.txt")).unwrap(), b"n");
    assert!(
        !fx.dst.join("gone.txt").exists(),
        "unrelated deletions still run"
    );
}

#[test]
fn source_file_vanishes_before_copy() {
    let fx = Fx::new();
    write(&fx.src, "a.txt", b"a");
    write(&fx.src, "b.txt", b"b");
    let src = fx.src.clone();
    let r = run_with(
        &fx,
        &fx.preset(),
        &RunOptions::default(),
        proceed(),
        &AtomicBool::new(false),
        &mut |e| {
            if let Event::FileStart { path, .. } = e
                && path == "a.txt"
            {
                let _ = fs::remove_file(src.join("b.txt"));
            }
        },
    );
    assert_eq!(r.stats.outcome, Outcome::Errors);
    assert!(r.stats.errors.iter().any(|e| e.path == "b.txt"));
    assert!(fx.dst.join("a.txt").exists());
}

#[test]
fn ctrl_c_mid_copy_stops_cleanly() {
    let fx = Fx::new();
    write(&fx.src, "a.txt", b"a");
    run(&fx, &fx.preset());
    write(&fx.dst, "extra.txt", b"x");
    for n in ["b", "c", "d"] {
        write(&fx.src, n, b"data");
    }
    let cancel = AtomicBool::new(false);
    let r = run_with(
        &fx,
        &fx.preset(),
        &RunOptions::default(),
        proceed(),
        &cancel,
        &mut |e| {
            if matches!(e, Event::FileStart { .. }) {
                cancel.store(true, Ordering::Relaxed);
            }
        },
    );
    assert_eq!(r.stats.outcome, Outcome::Interrupted);
    assert!(
        fx.dst.join("extra.txt").exists(),
        "no deletions after an interrupt"
    );
    assert!(files(&fx.dst).iter().all(|f| !f.contains(".bupr-tmp-")));
}

#[test]
fn dest_root_vanishes_mid_run() {
    let fx = Fx::new();
    write(&fx.src, "a.txt", b"a");
    run(&fx, &fx.preset());
    write(&fx.src, "sub/b.txt", b"b");
    write(&fx.src, "sub/c.txt", b"c");
    let dst = fx.dst.clone();
    let r = run_with(
        &fx,
        &fx.preset(),
        &RunOptions::default(),
        proceed(),
        &AtomicBool::new(false),
        &mut |e| {
            if let Event::FileStart { path, .. } = e
                && path == "sub/b.txt"
            {
                let _ = fs::remove_dir_all(&dst);
            }
        },
    );
    assert_eq!(r.stats.outcome, Outcome::Errors, "{:?}", r.stats);
    assert!(!fx.dst.exists());
    fx.assert_outside_untouched();
}

#[test]
fn dry_run_changes_nothing_and_never_asks() {
    let fx = Fx::new();
    write(&fx.src, "a.txt", b"a");
    let mut events = Vec::new();
    let stats = bupr::engine::run(
        &fx.preset(),
        &fx.env(),
        &RunOptions {
            mode: Mode::DryRun,
            list_plan: true,
        },
        &mut |e| events.push(e),
        &mut |_| panic!("a dry run must not ask for a decision"),
        &AtomicBool::new(false),
    );
    assert_eq!(stats.outcome, Outcome::Ok);
    assert!(!fx.dst.exists());
    assert!(events.contains(&Event::PlanItem {
        action: PlanAction::Copy,
        path: "a.txt".into()
    }));
}

#[test]
fn simulate_reads_everything_and_writes_nothing() {
    let fx = Fx::new();
    write(&fx.src, "a.bin", &vec![1u8; 3 << 20]);
    write(&fx.src, "d/b.txt", b"b");
    let r = run_with(
        &fx,
        &fx.preset(),
        &RunOptions {
            mode: Mode::Simulate,
            list_plan: false,
        },
        proceed(),
        &AtomicBool::new(false),
        &mut |_| {},
    );
    assert_eq!(r.stats.outcome, Outcome::Ok);
    assert!(
        r.summary.is_some(),
        "simulate goes through the decision step"
    );
    assert_eq!(
        (r.stats.copied_files, r.stats.copied_bytes),
        (2, (3 << 20) + 1)
    );
    let progressed: u64 = r
        .events
        .iter()
        .map(|e| {
            if let Event::FileProgress { bytes } = e {
                *bytes
            } else {
                0
            }
        })
        .sum();
    assert_eq!(progressed, (3 << 20) + 1);
    assert!(!fx.dst.exists());
}

#[test]
fn read_only_source_folders_round_trip() {
    let fx = Fx::new();
    write(&fx.src, "ro/a.txt", b"a");
    chmod(&fx.src.join("ro"), 0o555);
    run(&fx, &fx.preset());
    assert_eq!(
        fs::metadata(fx.dst.join("ro")).unwrap().mode() & 0o777,
        0o555
    );
    chmod(&fx.src.join("ro"), 0o755);
    write(&fx.src, "ro/b.txt", b"b");
    chmod(&fx.src.join("ro"), 0o555);
    let r = run(&fx, &fx.preset());
    assert_eq!(r.stats.outcome, Outcome::Ok, "{:?}", r.stats);
    assert!(fx.dst.join("ro/b.txt").exists());
}

#[test]
fn type_swaps_between_runs() {
    let fx = Fx::new();
    write(&fx.src, "x", b"file");
    write(&fx.src, "y/inner", b"dir");
    run(&fx, &fx.preset());
    fs::remove_file(fx.src.join("x")).unwrap();
    write(&fx.src, "x/child", b"now a dir");
    fs::remove_dir_all(fx.src.join("y")).unwrap();
    write(&fx.src, "y", b"now a file");
    let r = run(&fx, &fx.preset());
    assert_eq!(r.stats.outcome, Outcome::Ok, "{:?}", r.stats);
    assert_eq!(fs::read(fx.dst.join("x/child")).unwrap(), b"now a dir");
    assert_eq!(fs::read(fx.dst.join("y")).unwrap(), b"now a file");
}

fn no_temp_files(fx: &Fx) -> bool {
    files(&fx.dst).iter().all(|f| !f.contains(".bupr-tmp-"))
}

#[test]
fn an_interrupted_type_change_keeps_the_old_version() {
    let fx = Fx::new();
    write(&fx.src, "x", b"old file");
    write(&fx.src, "y/inner", b"old tree");
    write(&fx.src, "z", b"z");
    run(&fx, &fx.preset());
    fs::remove_file(fx.src.join("x")).unwrap();
    write(&fx.src, "x/a", b"new tree");
    write(&fx.src, "x/b", b"new tree");
    fs::remove_dir_all(fx.src.join("y")).unwrap();
    write(&fx.src, "y", b"new file");
    write(&fx.src, "z", b"z2");
    let cancel = AtomicBool::new(false);
    let r = run_with(
        &fx,
        &fx.preset(),
        &RunOptions::default(),
        proceed(),
        &cancel,
        &mut |e| {
            if matches!(e, Event::FileDone { path } if path == "y") {
                cancel.store(true, Ordering::Relaxed);
            }
        },
    );
    assert_eq!(r.stats.outcome, Outcome::Interrupted, "{:?}", r.stats);
    assert_eq!(fs::read(fx.dst.join("x")).unwrap(), b"old file");
    assert_eq!(fs::read(fx.dst.join("y/inner")).unwrap(), b"old tree");
    assert!(no_temp_files(&fx));
    let r = run(&fx, &fx.preset());
    assert_eq!(r.stats.outcome, Outcome::Ok, "{:?}", r.stats);
    assert_eq!(fs::read(fx.dst.join("x/b")).unwrap(), b"new tree");
    assert_eq!(fs::read(fx.dst.join("y")).unwrap(), b"new file");
    assert!(no_temp_files(&fx));
}

#[test]
fn a_replacement_that_fails_to_copy_keeps_the_old_version() {
    let fx = Fx::new();
    write(&fx.src, "y/inner", b"old tree");
    run(&fx, &fx.preset());
    fs::remove_dir_all(fx.src.join("y")).unwrap();
    write(&fx.src, "y", b"unreadable new file");
    chmod(&fx.src.join("y"), 0o000);
    let r = run(&fx, &fx.preset());
    chmod(&fx.src.join("y"), 0o644);
    assert_eq!(r.stats.outcome, Outcome::Errors, "{:?}", r.stats);
    assert_eq!(fs::read(fx.dst.join("y/inner")).unwrap(), b"old tree");
    assert_eq!(r.stats.deleted, 0);
    assert!(no_temp_files(&fx));
}

#[test]
fn read_only_files_are_copied() {
    let fx = Fx::new();
    write(&fx.src, "ro.txt", b"ro");
    chmod(&fx.src.join("ro.txt"), 0o444);
    let r = run(&fx, &fx.preset());
    assert_eq!(r.stats.outcome, Outcome::Ok, "{:?}", r.stats);
    assert_eq!(fs::read(fx.dst.join("ro.txt")).unwrap(), b"ro");
    assert_eq!(
        fs::metadata(fx.dst.join("ro.txt")).unwrap().mode() & 0o777,
        0o444
    );
    assert!(no_temp_files(&fx));
}

#[test]
fn a_replaced_folder_that_gained_entries_still_gets_the_new_version() {
    let fx = Fx::new();
    write(&fx.src, "x/a", b"old");
    write(&fx.src, "z", b"z");
    run(&fx, &fx.preset());
    fs::remove_dir_all(fx.src.join("x")).unwrap();
    write(&fx.src, "x", b"new file");
    write(&fx.src, "z", b"z2");
    let dst = fx.dst.clone();
    let r = run_with(
        &fx,
        &fx.preset(),
        &RunOptions::default(),
        proceed(),
        &AtomicBool::new(false),
        &mut |e| {
            if matches!(e, Event::FileStart { path, .. } if path == "z") {
                fs::write(dst.join("x/created-since-scan"), b"?").unwrap();
            }
        },
    );
    assert_eq!(r.stats.outcome, Outcome::Ok, "{:?}", r.stats);
    assert_eq!(fs::read(fx.dst.join("x")).unwrap(), b"new file");
    assert!(
        r.events
            .iter()
            .any(|e| matches!(e, Event::Warning { message } if message.contains("left in")))
    );
    // The set-aside old version is cleaned up by the next run.
    run(&fx, &fx.preset());
    assert!(no_temp_files(&fx));
}

#[test]
fn a_type_change_counts_toward_the_delete_limit() {
    let fx = Fx::new();
    write(&fx.src, "big.bin", &[1u8; 5000]);
    run(&fx, &fx.preset());
    fs::remove_file(fx.src.join("big.bin")).unwrap();
    symlink("elsewhere", &fx.src, "big.bin");
    let mut p = fx.preset();
    p.max_delete_bytes = 1000;
    let r = run_with(
        &fx,
        &p,
        &RunOptions::default(),
        Decision::Proceed {
            allow_deletes: false,
            adopt: false,
        },
        &AtomicBool::new(false),
        &mut |_| {},
    );
    let s = r.summary.unwrap();
    assert!(s.over_delete_limit);
    assert_eq!(s.totals.delete_bytes, 5000);
    assert_eq!(fs::read(fx.dst.join("big.bin")).unwrap(), [1u8; 5000]);
    let r = run(&fx, &p);
    assert_eq!(r.stats.outcome, Outcome::Ok, "{:?}", r.stats);
    assert_eq!(
        fs::read_link(fx.dst.join("big.bin")).unwrap().to_str(),
        Some("elsewhere")
    );
}

#[test]
fn a_source_file_swapped_for_a_symlink_after_the_scan_is_not_followed() {
    let fx = Fx::new();
    write(&fx.src, "a.txt", b"a");
    write(&fx.src, "b.txt", b"b");
    let (src, outside) = (fx.src.clone(), fx.outside.clone());
    let r = run_with(
        &fx,
        &fx.preset(),
        &RunOptions::default(),
        proceed(),
        &AtomicBool::new(false),
        &mut |e| {
            if matches!(e, Event::FileStart { path, .. } if path == "a.txt") {
                fs::remove_file(src.join("b.txt")).unwrap();
                std::os::unix::fs::symlink(outside.join("precious.txt"), src.join("b.txt"))
                    .unwrap();
            }
        },
    );
    assert_eq!(r.stats.outcome, Outcome::Errors, "{:?}", r.stats);
    let err = r.stats.errors.iter().find(|e| e.path == "b.txt").unwrap();
    assert!(err.message.contains("changed since the scan"), "{err:?}");
    assert!(!fx.dst.join("b.txt").exists());
}

#[test]
fn metadata_only_changes_reach_the_backup() {
    let fx = Fx::new();
    let vol = bupr::preflight::volume(&fx.root);
    if !(vol.nanos && vol.modes && vol.xattrs) {
        eprintln!("skipping: temp volume is not APFS");
        return;
    }
    write(&fx.src, "a.txt", b"one");
    let a = fx.src.join("a.txt");
    let t = std::time::UNIX_EPOCH + std::time::Duration::new(1_600_000_000, 100);
    let set = |t| {
        fs::File::open(&a)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(t))
            .unwrap()
    };
    set(t);
    run(&fx, &fx.preset());
    // Same size, same second: only the nanoseconds differ.
    fs::write(&a, b"two").unwrap();
    set(t + std::time::Duration::from_nanos(1));
    let r = run(&fx, &fx.preset());
    assert_eq!(r.stats.copied_files, 1);
    assert_eq!(fs::read(fx.dst.join("a.txt")).unwrap(), b"two");
    chmod(&a, 0o600);
    assert_eq!(run(&fx, &fx.preset()).stats.copied_files, 1);
    assert_eq!(
        fs::metadata(fx.dst.join("a.txt")).unwrap().mode() & 0o777,
        0o600
    );
    xattr::set(&a, "com.example.tag", b"red").unwrap();
    assert_eq!(run(&fx, &fx.preset()).stats.copied_files, 1);
    assert_eq!(
        xattr::get(fx.dst.join("a.txt"), "com.example.tag").unwrap(),
        Some(b"red".to_vec())
    );
    assert_eq!(run(&fx, &fx.preset()).stats.copied_files, 0);
}

#[test]
fn the_destination_root_mode_is_restored() {
    let fx = Fx::new();
    write(&fx.src, "a.txt", b"a");
    run(&fx, &fx.preset());
    chmod(&fx.dst, 0o555);
    write(&fx.src, "b.txt", b"b");
    let r = run(&fx, &fx.preset());
    assert_eq!(r.stats.outcome, Outcome::Ok, "{:?}", r.stats);
    assert!(fx.dst.join("b.txt").exists());
    assert_eq!(fs::metadata(&fx.dst).unwrap().mode() & 0o777, 0o555);
}

#[test]
fn skipped_special_files_are_reported_on_a_real_run() {
    let fx = Fx::new();
    write(&fx.src, "a.txt", b"a");
    let fifo = std::ffi::CString::new(fx.src.join("pipe").to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) }, 0);
    let r = run(&fx, &fx.preset());
    assert_eq!(r.stats.outcome, Outcome::Ok);
    assert!(
        r.events
            .iter()
            .any(|e| matches!(e, Event::Warning { message } if message.contains("special file")))
    );
}

#[test]
fn preflight_failure_is_reported_once() {
    let fx = Fx::new();
    let mut p = fx.preset();
    p.source = fx.root.join("missing");
    let r = run(&fx, &p);
    assert_eq!(r.stats.outcome, Outcome::PreflightFailed);
    assert!(
        r.stats
            .message
            .as_deref()
            .unwrap()
            .contains("does not exist")
    );
    assert!(r.events.iter().any(|e| matches!(e, Event::Fatal { .. })));
    assert!(!fx.dst.exists());
}

#[test]
fn apfs_case_folded_rename_keeps_the_file() {
    let fx = Fx::new();
    if !bupr::preflight::is_case_insensitive(&fx.root) {
        eprintln!("skipping: temp volume is case-sensitive");
        return;
    }
    write(&fx.src, "strasse.txt", b"v1");
    write(&fx.src, "Strasse/a.txt", b"a");
    run(&fx, &fx.preset());
    fs::rename(fx.src.join("strasse.txt"), fx.src.join("straße.txt")).unwrap();
    fs::rename(fx.src.join("Strasse"), fx.src.join("Straße")).unwrap();
    let r = run(&fx, &fx.preset());
    assert_eq!(r.stats.outcome, Outcome::Ok, "{:?}", r.stats);
    assert_eq!(r.stats.deleted, 0);
    assert_eq!(fs::read(fx.dst.join("straße.txt")).unwrap(), b"v1");
    assert_eq!(fs::read(fx.dst.join("Straße/a.txt")).unwrap(), b"a");
}

#[test]
fn entries_replaced_since_the_scan_are_not_deleted() {
    let fx = Fx::new();
    write(&fx.src, "a.txt", b"a");
    run(&fx, &fx.preset());
    write(&fx.dst, "extra.txt", b"old");
    write(&fx.src, "b.txt", b"b");
    let dst = fx.dst.clone();
    let r = run_with(
        &fx,
        &fx.preset(),
        &RunOptions::default(),
        proceed(),
        &AtomicBool::new(false),
        &mut |e| {
            if let Event::FileStart { path, .. } = e
                && path == "b.txt"
            {
                fs::remove_file(dst.join("extra.txt")).unwrap();
                fs::write(dst.join("extra.txt"), b"new file, new inode").unwrap();
            }
        },
    );
    assert_eq!(r.stats.deleted, 0, "{:?}", r.stats);
    assert_eq!(
        fs::read(fx.dst.join("extra.txt")).unwrap(),
        b"new file, new inode"
    );
}

#[test]
fn another_presets_backup_inside_the_destination_is_never_deleted() {
    let fx = Fx::new();
    write(&fx.src, "a.txt", b"a");
    run(&fx, &fx.preset());
    write(
        &fx.dst,
        "media/.bupr-dest",
        br#"{"preset":"media","source":"/m","created":"t"}"#,
    );
    write(&fx.dst, "media/movie.mp4", b"precious");
    let r = run(&fx, &fx.preset());
    assert_eq!(r.stats.outcome, Outcome::Errors, "{:?}", r.stats);
    assert_eq!(r.stats.deleted, 0);
    assert_eq!(
        fs::read(fx.dst.join("media/movie.mp4")).unwrap(),
        b"precious"
    );
}

#[test]
fn ctrl_c_during_the_decision_changes_nothing() {
    let fx = Fx::new();
    write(&fx.src, "x/inner", b"dir");
    write(&fx.src, "keep.txt", b"k");
    run(&fx, &fx.preset());
    fs::remove_dir_all(fx.src.join("x")).unwrap();
    write(&fx.src, "x", b"now a file");
    write(&fx.src, "newdir/f", b"f");
    let before = snapshot(&fx.dst);
    let cancel = AtomicBool::new(false);
    let stats = bupr::engine::run(
        &fx.preset(),
        &fx.env(),
        &RunOptions::default(),
        &mut |_| {},
        &mut |_| {
            cancel.store(true, Ordering::Relaxed);
            proceed()
        },
        &cancel,
    );
    assert_eq!(stats.outcome, Outcome::Interrupted);
    assert_eq!(stats.deleted, 0);
    assert_eq!(snapshot(&fx.dst), before, "nothing may change after Ctrl-C");
}
