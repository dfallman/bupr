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

/// What the dynamic loader and libSystem read: the shared cache and its
/// cryptex, Rosetta, and `/dev`. dyld also reads `/` itself (without it,
/// every process aborts at launch), so the profile allows that literal.
const SYSTEM_READS: &[&str] = &[
    "/System",
    "/usr/lib",
    "/private/preboot",
    "/private/var/db/dyld",
    "/Library/Apple",
    "/private/var/db/oah",
    "/dev",
];

/// Quote a path as an SBPL string. Control characters (a newline ends the
/// string) and non-UTF-8 names (a lossy conversion names another path) are
/// refused rather than escaped.
fn sbpl_quote(p: &Path) -> Result<String, String> {
    let s = p
        .to_str()
        .filter(|s| !s.chars().any(|c| c.is_ascii_control()))
        .ok_or_else(|| format!("cannot sandbox {p:?}: control character or invalid UTF-8"))?;
    Ok(format!(
        "\"{}\"",
        s.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

/// Kernel sandbox profile for one preset. No network. File contents are
/// readable only in `source`, `dest`, its `missing` ancestors, the worker
/// executable and system libraries; metadata stays readable everywhere (path
/// resolution, volume checks). Writes only inside `dest` and creation of
/// `missing`, and none unless `write` (dry run, simulate).
///
/// SBPL is last-match-wins, except that a rule naming an operation beats a
/// wildcard (`file-read-metadata` over `file-read*`): a later
/// `(allow file-read* ...)` would not undo a `(deny file-read-data)`.
pub fn sandbox_profile(
    source: &Path,
    dest: &Path,
    missing: &[PathBuf],
    write: bool,
) -> Result<String, String> {
    let exe = std::env::current_exe().ok();
    let exes = exe
        .iter()
        .flat_map(|e| [Some(e.clone()), e.canonicalize().ok().filter(|c| c != e)])
        .flatten();
    let mut s = String::from(
        "(version 1)\n(allow default)\n(deny network*)\n(deny file-read*)\n\
         (allow file-read-metadata)\n(allow file-read*\n  (literal \"/\")\n",
    );
    for r in SYSTEM_READS {
        s += &format!("  (subpath \"{r}\")\n");
    }
    for p in [source, dest] {
        s += &format!("  (subpath {})\n", sbpl_quote(p)?);
    }
    for p in missing.iter().cloned().chain(exes) {
        s += &format!("  (literal {})\n", sbpl_quote(&p)?);
    }
    s += ")\n(deny file-write*)\n";
    if write {
        s += &format!("(allow file-write* (subpath {}))\n", sbpl_quote(dest)?);
        for d in missing {
            s += &format!("(allow file-write-create (literal {}))\n", sbpl_quote(d)?);
        }
    }
    s += "(allow file-write-data (literal \"/dev/null\"))\n";
    Ok(s)
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

    fn profile(dest: &str, write: bool) -> Result<String, String> {
        let media = PathBuf::from("/Volumes/B/media");
        sandbox_profile(Path::new("/src"), Path::new(dest), &[media], write)
    }

    #[test]
    fn profile_allows_only_the_destination_and_missing_ancestors() {
        let p = profile("/Volumes/B/media/video", true).unwrap();
        assert!(p.contains("(deny network*)"));
        assert!(p.contains("(deny file-write*)"));
        assert!(p.contains("(allow file-write* (subpath \"/Volumes/B/media/video\"))"));
        assert!(p.contains("(allow file-write-create (literal \"/Volumes/B/media\"))"));
        assert!(p.contains("  (subpath \"/src\")\n  (subpath \"/Volumes/B/media/video\")\n"));
        assert!(p.contains("  (literal \"/Volumes/B/media\")\n"));
    }

    #[test]
    fn read_only_profile_allows_no_writes() {
        let p = profile("/Volumes/B/media/video", false).unwrap();
        assert!(p.contains("(deny file-write*)"));
        assert!(!p.contains("(allow file-write* "));
        assert!(!p.contains("file-write-create"));
    }

    #[test]
    fn quoting_escapes_quotes_and_backslashes() {
        let p = profile("/x/a\"b\\c", true).unwrap();
        assert!(p.contains(r#"(subpath "/x/a\"b\\c")"#), "{p}");
    }

    #[test]
    fn control_characters_and_invalid_utf8_are_refused() {
        for bad in ["/x/a\nb", "/x/a\rb", "/x/a\0b", "/x/a\x7fb", "/x/a\x1bb"] {
            assert!(profile(bad, true).is_err(), "{bad:?}");
        }
        use std::os::unix::ffi::OsStrExt;
        let p = Path::new(std::ffi::OsStr::from_bytes(b"/x/\xff"));
        assert!(sandbox_profile(p, Path::new("/d"), &[], false).is_err());
    }
}
