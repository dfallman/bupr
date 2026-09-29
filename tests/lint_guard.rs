//! Enforces the module boundary of the write-API ban (spec §3.3).

use std::fs;
use std::path::{Path, PathBuf};

const ALLOWED: &[&str] = &["src/dest.rs", "src/state.rs", "src/testutil.rs"];

fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            rs_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn only_dest_state_and_testutil_opt_out_of_the_write_ban() {
    let mut files = Vec::new();
    rs_files(&root().join("src"), &mut files);
    let offenders: Vec<String> = files
        .iter()
        .map(|f| {
            f.strip_prefix(root())
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .filter(|rel| {
            fs::read_to_string(root().join(rel))
                .unwrap()
                .contains("disallowed_methods")
                && !ALLOWED.contains(&rel.as_str())
        })
        .collect();
    assert!(
        offenders.is_empty(),
        "write-API ban bypassed in: {offenders:?}"
    );
}

#[test]
fn testutil_is_compiled_only_for_tests() {
    let lib = fs::read_to_string(root().join("src/lib.rs")).unwrap();
    assert!(lib.contains("#[cfg(test)]\nmod testutil;"));
    assert!(!lib.contains("pub mod testutil"));
}

#[test]
fn clippy_bans_the_core_write_apis() {
    let cfg = fs::read_to_string(root().join("clippy.toml")).unwrap();
    for api in [
        "std::fs::write",
        "std::fs::remove_file",
        "std::fs::remove_dir_all",
        "std::fs::rename",
        "std::fs::OpenOptions::new",
        "std::fs::File::create",
        "cap_std::fs::Dir::remove_dir_all",
        "cap_std::fs::Dir::open_with",
        "xattr::FileExt::set_xattr",
    ] {
        assert!(
            cfg.contains(&format!("\"{api}\"")),
            "clippy.toml must ban {api}"
        );
    }
}
