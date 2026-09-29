#![allow(clippy::disallowed_methods)]

mod common;

use std::process::Command;

use bupr::audit::audit;
use common::*;

fn git(dir: &std::path::Path, args: &[&str]) {
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .unwrap()
            .success()
    );
}

#[test]
fn audit_reports_sizes_reasons_and_gitignore_hints() {
    let fx = Fx::new();
    write(&fx.src, "p/Cargo.toml", b"x");
    write(&fx.src, "p/target/big", &[0u8; 1000]);
    write(&fx.src, "p/src/a.rs", &[0u8; 10]);
    write(&fx.src, "p/.gitignore", b"target/\ncache/\n");
    write(&fx.src, "p/cache/blob", &[0u8; 500]);
    write(&fx.src, "p/CLAUDE.md", b"notes");
    git(&fx.src.join("p"), &["init", "-q"]);

    let rep = audit(&fx.preset(), 100).unwrap();

    assert!(
        rep.excluded
            .contains(&("target/ next to Cargo.toml".to_string(), 1000)),
        "{:?}",
        rep.excluded
    );
    assert_eq!(rep.hints.len(), 1, "{:?}", rep.hints);
    assert_eq!(rep.hints[0].path, "p/cache");
    assert_eq!(rep.hints[0].bytes, 500);
    assert_eq!(rep.hints[0].suggestion, "/p/cache/");
    assert_eq!(rep.top_dirs[0].0, "p");
    assert!(rep.included_bytes >= 515);
}
