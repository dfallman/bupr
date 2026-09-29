#![allow(clippy::disallowed_methods)]

mod common;

use std::fs;
use std::path::PathBuf;

use assert_cmd::Command;
use common::*;
use predicates::prelude::*;

struct CliFx {
    fx: Fx,
    config: PathBuf,
}

fn cli_fx(extra: &str) -> CliFx {
    let fx = Fx::new();
    let config = fx.root.join("config.toml");
    fs::write(
        &config,
        format!(
            "[presets.dev]\nsource = {:?}\ndestination = {:?}\nrules = [\"dev\"]\nallow_internal = true\n{extra}",
            fx.src, fx.dst
        ),
    )
    .unwrap();
    CliFx { fx, config }
}

fn bupr(c: &CliFx) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_bupr"));
    cmd.env("HOME", c.fx.root.join("home"))
        .env("XDG_STATE_HOME", c.fx.root.join("state"))
        .env("NO_COLOR", "1")
        .env_remove("XDG_CONFIG_HOME")
        .arg("--config")
        .arg(&c.config);
    cmd
}

fn history(c: &CliFx) -> String {
    fs::read_to_string(c.fx.root.join("state/bupr/history.jsonl")).unwrap_or_default()
}

#[test]
fn runs_a_preset_and_records_history() {
    let c = cli_fx("");
    write(&c.fx.src, "a.txt", b"a");
    bupr(&c)
        .arg("dev")
        .assert()
        .success()
        .stdout(predicate::str::contains("✓ dev"));
    assert_eq!(fs::read(c.fx.dst.join("a.txt")).unwrap(), b"a");
    let h = history(&c);
    assert_eq!(h.lines().count(), 1);
    assert!(h.contains("\"outcome\":\"ok\"") && h.contains("\"mode\":\"run\""));
}

#[test]
fn dry_run_changes_nothing_and_lists_paths() {
    let c = cli_fx("");
    write(&c.fx.src, "a.txt", b"a");
    bupr(&c)
        .args(["dev", "--dry-run", "-v"])
        .assert()
        .success()
        .stdout(predicate::str::contains("copy").and(predicate::str::contains("+ a.txt")));
    assert!(!c.fx.dst.exists());
}

#[test]
fn simulate_reads_but_writes_nothing() {
    let c = cli_fx("");
    write(&c.fx.src, "a.txt", b"abc");
    bupr(&c)
        .args(["dev", "--simulate"])
        .assert()
        .success()
        .stdout(predicate::str::contains("(simulated)"));
    assert!(!c.fx.dst.exists());
    assert!(history(&c).contains("\"mode\":\"simulate\""));
}

#[test]
fn piped_output_has_no_escape_codes() {
    let c = cli_fx("");
    write(&c.fx.src, "a.txt", b"a");
    let out = bupr(&c).arg("dev").output().unwrap();
    assert!(!String::from_utf8_lossy(&out.stdout).contains('\u{1b}'));
    assert!(!String::from_utf8_lossy(&out.stderr).contains('\u{1b}'));
}

#[test]
fn unknown_preset_exits_2() {
    let c = cli_fx("");
    bupr(&c)
        .arg("nope")
        .assert()
        .code(2)
        .stderr(predicate::str::contains("unknown preset"));
}

#[test]
fn unmounted_drive_is_refused() {
    let fx = Fx::new();
    let config = fx.root.join("config.toml");
    fs::write(
        &config,
        format!("[presets.dev]\nsource = {:?}\ndestination = \"/Volumes/bupr-test-no-such-volume/dev\"\n", fx.src),
    )
    .unwrap();
    let c = CliFx { fx, config };
    bupr(&c)
        .arg("dev")
        .assert()
        .code(2)
        .stdout(predicate::str::contains("drive not mounted"));
    assert!(!PathBuf::from("/Volumes/bupr-test-no-such-volume").exists());
}

#[test]
fn destination_inside_source_is_refused() {
    let fx = Fx::new();
    let config = fx.root.join("config.toml");
    fs::write(
        &config,
        format!(
            "[presets.dev]\nsource = {:?}\ndestination = {:?}\nallow_internal = true\n",
            fx.src,
            fx.src.join("bk")
        ),
    )
    .unwrap();
    let c = CliFx { fx, config };
    bupr(&c)
        .arg("dev")
        .assert()
        .code(2)
        .stdout(predicate::str::contains("overlaps"));
    assert!(!c.fx.src.join("bk").exists());
}

#[test]
fn unattended_run_skips_deletions_over_the_limit() {
    let c = cli_fx("max_delete = 1\n");
    write(&c.fx.src, "a.txt", b"a");
    bupr(&c).arg("dev").assert().success();
    for n in ["x1", "x2", "x3"] {
        write(&c.fx.dst, n, b"x");
    }
    bupr(&c)
        .arg("dev")
        .assert()
        .code(1)
        .stdout(predicate::str::contains("deletions skipped"));
    assert!(c.fx.dst.join("x1").exists());
}

#[test]
fn unattended_run_never_adopts_a_foreign_folder() {
    let c = cli_fx("");
    write(&c.fx.src, "a.txt", b"a");
    write(&c.fx.dst, "theirs.txt", b"t");
    bupr(&c).arg("dev").assert().code(2);
    assert_eq!(files(&c.fx.dst), ["theirs.txt"]);
}

#[test]
fn invalid_config_names_the_bad_key() {
    let c = cli_fx("exlude = []\n");
    bupr(&c)
        .arg("dev")
        .assert()
        .code(2)
        .stderr(predicate::str::contains("exlude"));
}

#[test]
fn missing_config_explains_init() {
    let mut c = cli_fx("");
    c.config = c.fx.root.join("nope.toml");
    bupr(&c)
        .arg("dev")
        .assert()
        .code(2)
        .stderr(predicate::str::contains("bupr init"));
}

#[test]
fn bare_bupr_without_a_terminal_lists_presets() {
    let c = cli_fx("");
    bupr(&c)
        .assert()
        .code(2)
        .stdout(predicate::str::contains("dev"));
}

#[test]
fn dry_run_and_simulate_conflict() {
    let c = cli_fx("");
    bupr(&c)
        .args(["dev", "--dry-run", "--simulate"])
        .assert()
        .code(2);
}

#[test]
fn several_presets_and_all() {
    let c = cli_fx(
        "\n[presets.gone]\nsource = \"/tmp\"\ndestination = \"/Volumes/bupr-test-no-such-volume/x\"\n",
    );
    write(&c.fx.src, "a.txt", b"a");
    bupr(&c)
        .arg("--all")
        .assert()
        .success()
        .stdout(predicate::str::contains("skipping gone").and(predicate::str::contains("✓ dev")));
    bupr(&c).args(["dev", "gone"]).assert().code(2);
}

#[test]
fn rules_lists_the_packs() {
    let c = cli_fx("");
    bupr(&c).arg("rules").assert().success().stdout(
        predicate::str::contains("target/ next to Cargo.toml")
            .and(predicate::str::contains("junk")),
    );
}

#[test]
fn init_writes_a_starter_config_once() {
    let mut c = cli_fx("");
    c.config = c.fx.root.join("fresh/config.toml");
    bupr(&c).arg("init").assert().success();
    assert!(
        fs::read_to_string(&c.config)
            .unwrap()
            .contains("[presets.dev]")
    );
    bupr(&c)
        .arg("init")
        .assert()
        .code(2)
        .stderr(predicate::str::contains("already exists"));
}

#[test]
fn list_shows_status_and_log_shows_runs() {
    let c = cli_fx(
        "\n[presets.gone]\nsource = \"/tmp\"\ndestination = \"/Volumes/bupr-test-no-such-volume/x\"\n",
    );
    write(&c.fx.src, "a.txt", b"a");
    bupr(&c).arg("list").assert().success().stdout(
        predicate::str::contains("never").and(predicate::str::contains("(drive not mounted)")),
    );
    bupr(&c).arg("dev").assert().success();
    bupr(&c)
        .arg("log")
        .assert()
        .success()
        .stdout(predicate::str::contains("dev").and(predicate::str::contains("ok")));
    bupr(&c)
        .args(["log", "gone"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No runs"));
}

#[test]
fn edit_validates_before_saving() {
    let c = cli_fx("");
    let before = fs::read_to_string(&c.config).unwrap();
    let good = c.fx.root.join("good-editor.sh");
    fs::write(&good, "#!/bin/sh\nprintf '\\n# edited\\n' >> \"$1\"\n").unwrap();
    chmod(&good, 0o755);
    bupr(&c)
        .arg("edit")
        .env("EDITOR", &good)
        .env_remove("VISUAL")
        .assert()
        .success();
    assert!(
        fs::read_to_string(&c.config)
            .unwrap()
            .ends_with("# edited\n")
    );

    let bad = c.fx.root.join("bad-editor.sh");
    fs::write(&bad, "#!/bin/sh\nprintf 'bogus = 1\\n' >> \"$1\"\n").unwrap();
    chmod(&bad, 0o755);
    bupr(&c)
        .arg("edit")
        .env("EDITOR", &bad)
        .env_remove("VISUAL")
        .assert()
        .code(2)
        .stderr(predicate::str::contains("bogus"));
    assert_eq!(
        fs::read_to_string(&c.config).unwrap(),
        format!("{before}\n# edited\n")
    );
    assert!(!c.fx.root.join("config.toml.edit").exists());
}

#[test]
fn all_reports_a_missing_source_instead_of_skipping_it() {
    let c = cli_fx(
        "\n[presets.moved]\nsource = \"/nonexistent/bupr-moved\"\ndestination = \"/Volumes/whatever/x\"\n",
    );
    write(&c.fx.src, "a.txt", b"a");
    bupr(&c)
        .arg("--all")
        .assert()
        .code(2)
        .stdout(predicate::str::contains("✗ moved"));
    let h = history(&c);
    assert!(
        h.contains("\"preset\":\"moved\"") && h.contains("preflight_failed"),
        "{h}"
    );
}
