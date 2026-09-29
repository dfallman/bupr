//! The worker process: runs the engine for one preset, under a kernel
//! sandbox when available, speaking JSON lines with the parent
//! (spec §3.8, §7.1). Worker → parent: `Event` per line on stdout.
//! Parent → worker: `WorkerInit` as the first stdin line, then one `Decision`.

use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

use crate::config::Preset;
use crate::engine::{self, Decision, Event, PlanSummary, RunOptions};
use crate::preflight::Env;

pub const WORKER_ARG: &str = "__worker";
const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkerInit {
    pub preset: Preset,
    pub opts: RunOptions,
    pub env: Env,
}

fn sbpl_quote(p: &Path) -> String {
    let s = p.to_string_lossy();
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Kernel sandbox profile: deny every file write except inside `write_root`
/// and the creation of `create_dirs`. `None` denies all writes (dry run,
/// simulate).
pub fn sandbox_profile(write_root: Option<&Path>, create_dirs: &[PathBuf]) -> String {
    let mut s = String::from("(version 1)\n(allow default)\n(deny file-write*)\n");
    if let Some(root) = write_root {
        s += &format!("(allow file-write* (subpath {}))\n", sbpl_quote(root));
        for d in create_dirs {
            s += &format!("(allow file-write-create (literal {}))\n", sbpl_quote(d));
        }
    }
    s += "(allow file-write-data (literal \"/dev/null\"))\n";
    s
}

pub fn sandbox_available() -> bool {
    Command::new(SANDBOX_EXEC)
        .args(["-p", "(version 1)(allow default)", "/usr/bin/true"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

pub struct Worker {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Worker {
    pub fn spawn(init: &WorkerInit, profile: Option<&str>) -> io::Result<Worker> {
        let exe = std::env::current_exe()?;
        let mut cmd = match profile {
            Some(p) => {
                let mut c = Command::new(SANDBOX_EXEC);
                c.arg("-p").arg(p).arg(&exe);
                c
            }
            None => Command::new(&exe),
        };
        cmd.arg(WORKER_ARG)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = cmd.spawn()?;
        let mut stdin = child.stdin.take().expect("piped stdin");
        let stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
        writeln!(stdin, "{}", serde_json::to_string(init)?)?;
        stdin.flush()?;
        Ok(Worker {
            child,
            stdin,
            stdout,
        })
    }

    /// Next event, or `None` once the worker has closed its output.
    pub fn next_event(&mut self) -> io::Result<Option<Event>> {
        let mut line = String::new();
        if self.stdout.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        serde_json::from_str(&line)
            .map(Some)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    pub fn send(&mut self, d: &Decision) -> io::Result<()> {
        writeln!(self.stdin, "{}", serde_json::to_string(d)?)?;
        self.stdin.flush()
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
    }

    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.wait()
    }
}

/// Entry point of `bupr __worker`.
pub fn worker_main() -> i32 {
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = cancel.clone();
    // First Ctrl-C: stop cleanly after the current chunk. Second: exit now.
    let _ = ctrlc::set_handler(move || {
        if flag.swap(true, Ordering::SeqCst) {
            std::process::exit(2);
        }
    });
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let mut line = String::new();
    if input.read_line(&mut line).is_err() {
        return 2;
    }
    let init: WorkerInit = match serde_json::from_str(&line) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("bupr worker: invalid init message: {e}");
            return 2;
        }
    };
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut emit = |ev: Event| {
        if let Ok(s) = serde_json::to_string(&ev) {
            let _ = writeln!(out, "{s}");
            let _ = out.flush();
        }
    };
    let mut decide = |_: &PlanSummary| {
        let mut l = String::new();
        match input.read_line(&mut l) {
            Ok(n) if n > 0 => serde_json::from_str(&l).unwrap_or(Decision::Abort),
            _ => Decision::Abort,
        }
    };
    let stats = engine::run(
        &init.preset,
        &init.env,
        &init.opts,
        &mut emit,
        &mut decide,
        &cancel,
    );
    stats.outcome.exit_code()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_allows_only_the_destination_and_missing_ancestors() {
        let p = sandbox_profile(
            Some(Path::new("/Volumes/B/media/video")),
            &[PathBuf::from("/Volumes/B/media")],
        );
        assert!(p.contains("(deny file-write*)"));
        assert!(p.contains("(allow file-write* (subpath \"/Volumes/B/media/video\"))"));
        assert!(p.contains("(allow file-write-create (literal \"/Volumes/B/media\"))"));
    }

    #[test]
    fn read_only_profile_allows_no_writes() {
        let p = sandbox_profile(None, &[]);
        assert!(p.contains("(deny file-write*)"));
        assert!(!p.contains("subpath"));
    }

    #[test]
    fn quoting_escapes_quotes_and_backslashes() {
        let p = sandbox_profile(Some(Path::new("/x/a\"b\\c")), &[]);
        assert!(p.contains(r#"(subpath "/x/a\"b\\c")"#), "{p}");
    }
}
