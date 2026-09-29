//! Parent-side orchestration (spec §6, §7.1): preflight, spawn the sandboxed
//! worker, show progress, answer decisions, record history. Never touches the
//! source or the destination itself.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use crate::config::{Config, Preset};
use crate::engine::{Decision, Event, Mode, Outcome, PlanSummary, RunOptions, RunStats};
use crate::history::RunRecord;
use crate::menu_reason;
use crate::preflight::{self, Env};
use crate::state;
use crate::ui::dashboard::{self, DashState, Dashboard};
use crate::ui::format::{count, tilde};
use crate::ui::plain::{self, Plain};
use crate::worker::{self, Worker, WorkerInit};

/// Ctrl-C presses seen by the parent (the worker handles its own).
pub static INTERRUPTS: AtomicUsize = AtomicUsize::new(0);

pub fn interrupted() -> bool {
    INTERRUPTS.load(Ordering::SeqCst) > 0
}

pub trait Prompter {
    fn adopt(&mut self, preset: &Preset, s: &PlanSummary) -> bool;
    fn deletions(&mut self, preset: &Preset, s: &PlanSummary) -> DeleteChoice;
    fn low_space(&mut self, preset: &Preset, s: &PlanSummary) -> bool;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeleteChoice {
    Delete,
    Skip,
    Abort,
}

pub struct Ctx {
    pub config: Config,
    pub env: Env,
    pub history_path: PathBuf,
    pub unattended: bool,
    pub quiet: bool,
    pub color: bool,
    pub mode: Mode,
    pub verbose: bool,
}

/// Spec §6 step 5. Unattended runs always take the safe option.
pub fn decide(
    s: &PlanSummary,
    preset: &Preset,
    unattended: bool,
    prompter: &mut dyn Prompter,
) -> Decision {
    let mut adopt = false;
    if s.marker.needs_adoption() {
        if unattended || !prompter.adopt(preset, s) {
            return Decision::Abort;
        }
        adopt = true;
    }
    if s.insufficient_space && (unattended || !prompter.low_space(preset, s)) {
        return Decision::Abort;
    }
    let mut allow_deletes = true;
    if s.over_delete_limit {
        if unattended {
            allow_deletes = false;
        } else {
            match prompter.deletions(preset, s) {
                DeleteChoice::Delete => {}
                DeleteChoice::Skip => allow_deletes = false,
                DeleteChoice::Abort => return Decision::Abort,
            }
        }
    }
    Decision::Proceed {
        allow_deletes,
        adopt,
    }
}

enum View {
    Dash {
        live: Option<Box<Dashboard>>,
        parked: Option<DashState>,
        start: Instant,
        color: bool,
    },
    Plain(Plain),
}

impl View {
    fn new(ctx: &Ctx, preset: &Preset) -> View {
        let wide =
            ratatui::crossterm::terminal::size().is_ok_and(|(w, _)| w >= dashboard::MIN_WIDTH);
        if !ctx.unattended
            && !ctx.quiet
            && ctx.mode != Mode::DryRun
            && wide
            && std::io::stdout().is_terminal()
        {
            let start = Instant::now();
            if let Ok(d) = Dashboard::new(DashState::new(&preset.name, ctx.mode), start, ctx.color)
            {
                return View::Dash {
                    live: Some(Box::new(d)),
                    parked: None,
                    start,
                    color: ctx.color,
                };
            }
        }
        View::Plain(Plain::new(&preset.name, ctx.mode, ctx.quiet))
    }

    fn event(&mut self, ev: &Event) {
        match self {
            View::Dash { live: Some(d), .. } => {
                let _ = d.event(ev);
            }
            View::Dash {
                parked: Some(s), ..
            } => s.apply(ev),
            View::Dash { .. } => {}
            View::Plain(p) => p.event(ev),
        }
    }

    fn note(&mut self, text: &str) {
        match self {
            View::Dash { live: Some(d), .. } => {
                let _ = d.note(text);
            }
            _ => println!("{text}"),
        }
    }

    fn suspend(&mut self) {
        if let View::Dash { live, parked, .. } = self
            && let Some(d) = live.take()
        {
            *parked = Some(d.close());
        }
    }

    fn resume(&mut self) {
        if let View::Dash {
            live,
            parked,
            start,
            color,
        } = self
            && let Some(s) = parked.take()
        {
            match Dashboard::new(s.clone(), *start, *color) {
                Ok(d) => *live = Some(Box::new(d)),
                Err(_) => *parked = Some(s),
            }
        }
    }
}

fn failed(mode: Mode, outcome: Outcome, message: String) -> RunStats {
    let mut s = RunStats::new(mode);
    s.outcome = outcome;
    s.message = Some(message);
    s
}

fn execute_one(ctx: &Ctx, preset: &Preset, sandbox: bool, prompter: &mut dyn Prompter) -> RunStats {
    let resolved = match preflight::check_paths(preset, &ctx.env) {
        Ok(r) => r,
        Err(e) => return failed(ctx.mode, Outcome::PreflightFailed, e.to_string()),
    };
    if !ctx.quiet {
        println!(
            "bupr · {}  {} → {}",
            preset.name,
            tilde(&resolved.source, &ctx.env.home),
            tilde(&resolved.dest, &ctx.env.home)
        );
    }
    let encrypted = if ctx.mode == Mode::Run {
        preflight::mount_point(&resolved.dest).and_then(|m| preflight::is_encrypted(&m))
    } else {
        None
    };
    let profile = sandbox.then(|| match ctx.mode {
        Mode::Run => worker::sandbox_profile(Some(&resolved.dest), &resolved.missing_ancestors),
        Mode::DryRun | Mode::Simulate => worker::sandbox_profile(None, &[]),
    });
    let init = WorkerInit {
        preset: preset.clone(),
        opts: RunOptions {
            mode: ctx.mode,
            list_plan: ctx.verbose && ctx.mode == Mode::DryRun,
        },
        env: ctx.env.clone(),
    };
    let mut worker = match Worker::spawn(&init, profile.as_deref()) {
        Ok(w) => w,
        Err(e) => {
            return failed(
                ctx.mode,
                Outcome::Aborted,
                format!("cannot start the copy worker: {e}"),
            );
        }
    };
    let mut view = View::new(ctx, preset);
    let mut done = None;
    loop {
        let ev = match worker.next_event() {
            Ok(Some(ev)) => ev,
            Ok(None) => break,
            Err(e) => {
                view.note(&format!("  ! worker: {e}"));
                break;
            }
        };
        match &ev {
            Event::Planned { summary } => {
                view.event(&ev);
                if !ctx.quiet {
                    if ctx.mode == Mode::DryRun {
                        for l in plain::plan_lines(preset, summary, &ctx.env.home) {
                            view.note(&l);
                        }
                    } else {
                        view.note(&plain::planned_line(summary));
                    }
                }
                if summary.secret_files > 0 && encrypted == Some(false) {
                    view.note(&format!(
                        "  ⚠ {} secret file(s) (.env, keys) will be copied to an unencrypted drive",
                        count(summary.secret_files)
                    ));
                }
                if ctx.mode != Mode::DryRun {
                    let ask = !ctx.unattended && summary.needs_prompt();
                    if ask {
                        view.suspend();
                    }
                    let d = decide(summary, preset, ctx.unattended, prompter);
                    if ask {
                        view.resume();
                    }
                    if worker.send(&d).is_err() {
                        worker.kill();
                    }
                }
            }
            Event::Done { stats } => {
                view.event(&ev);
                done = Some(stats.clone());
            }
            _ => view.event(&ev),
        }
    }
    let status = worker.wait();
    view.suspend();
    done.unwrap_or_else(|| {
        let why = status.map_or_else(|e| e.to_string(), |s| s.to_string());
        failed(
            ctx.mode,
            Outcome::Aborted,
            format!("the copy worker stopped unexpectedly ({why})"),
        )
    })
}

fn run_one(ctx: &Ctx, preset: &Preset, sandbox: bool, prompter: &mut dyn Prompter) -> Outcome {
    let started = jiff::Timestamp::now();
    let clock = Instant::now();
    let stats = execute_one(ctx, preset, sandbox, prompter);
    println!(
        "{}",
        plain::summary_line(&preset.name, &stats, clock.elapsed().as_secs())
    );
    for l in plain::error_lines(&stats, 10) {
        println!("{l}");
    }
    let rec = RunRecord::new(
        &preset.name,
        started,
        jiff::Timestamp::now(),
        ctx.unattended,
        &stats,
    );
    if let Err(e) = state::append_history(&ctx.history_path, &rec) {
        eprintln!(
            "  ! could not record history in {}: {e}",
            ctx.history_path.display()
        );
    }
    stats.outcome
}

pub fn run_presets(ctx: &Ctx, names: &[String], prompter: &mut dyn Prompter) -> i32 {
    let sandbox = worker::sandbox_available();
    if !sandbox {
        eprintln!("⚠ kernel sandbox unavailable; running with in-process safety only");
    }
    let mut code = 0;
    for (i, name) in names.iter().enumerate() {
        if interrupted() {
            break;
        }
        if i > 0 && !ctx.quiet {
            println!();
        }
        let Some(preset) = ctx.config.get(name) else {
            let known: Vec<&str> = ctx.config.presets.iter().map(|p| p.name.as_str()).collect();
            eprintln!(
                "✗ unknown preset {name:?}; known presets: {}",
                known.join(", ")
            );
            code = code.max(2);
            continue;
        };
        let outcome = run_one(ctx, preset, sandbox, prompter);
        code = code.max(outcome.exit_code());
        if outcome == Outcome::Interrupted {
            break;
        }
    }
    code
}

/// Presets whose paths pass preflight, and the others with a short reason.
pub fn available_presets(ctx: &Ctx) -> (Vec<String>, Vec<(String, String)>) {
    let mut ok = Vec::new();
    let mut skipped = Vec::new();
    for p in &ctx.config.presets {
        match preflight::check_paths(p, &ctx.env) {
            Ok(_) => ok.push(p.name.clone()),
            Err(e) => skipped.push((p.name.clone(), menu_reason(&e))),
        }
    }
    (ok, skipped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::Totals;
    use crate::preflight::MarkerStatus;

    struct Fake {
        adopt: bool,
        delete: DeleteChoice,
        space: bool,
        asked: Vec<&'static str>,
    }

    impl Prompter for Fake {
        fn adopt(&mut self, _: &Preset, _: &PlanSummary) -> bool {
            self.asked.push("adopt");
            self.adopt
        }
        fn deletions(&mut self, _: &Preset, _: &PlanSummary) -> DeleteChoice {
            self.asked.push("deletions");
            self.delete
        }
        fn low_space(&mut self, _: &Preset, _: &PlanSummary) -> bool {
            self.asked.push("space");
            self.space
        }
    }

    fn fake() -> Fake {
        Fake {
            adopt: true,
            delete: DeleteChoice::Delete,
            space: true,
            asked: vec![],
        }
    }

    fn summary() -> PlanSummary {
        PlanSummary {
            source: "/s".into(),
            dest: "/d".into(),
            totals: Totals::default(),
            marker: MarkerStatus::Matches,
            free_bytes: Some(100),
            needed_bytes: 1,
            over_delete_limit: false,
            insufficient_space: false,
            secret_files: 0,
            largest_deletes: vec![],
            skipped_special: 0,
            skipped_mounts: 0,
            collisions: 0,
            scan_errors: 0,
            case_insensitive: true,
        }
    }

    fn preset() -> Preset {
        Preset::minimal("dev", "/s".into(), "/d".into())
    }

    const GO: Decision = Decision::Proceed {
        allow_deletes: true,
        adopt: false,
    };

    #[test]
    fn clean_plan_proceeds_without_asking() {
        let mut f = fake();
        assert_eq!(decide(&summary(), &preset(), false, &mut f), GO);
        assert!(f.asked.is_empty());
    }

    #[test]
    fn foreign_destination() {
        let mut s = summary();
        s.marker = MarkerStatus::Foreign;
        assert_eq!(decide(&s, &preset(), true, &mut fake()), Decision::Abort);
        assert_eq!(
            decide(&s, &preset(), false, &mut fake()),
            Decision::Proceed {
                allow_deletes: true,
                adopt: true
            }
        );
        let mut no = fake();
        no.adopt = false;
        assert_eq!(decide(&s, &preset(), false, &mut no), Decision::Abort);
    }

    #[test]
    fn deletion_limit() {
        let mut s = summary();
        s.over_delete_limit = true;
        let skip = Decision::Proceed {
            allow_deletes: false,
            adopt: false,
        };
        assert_eq!(decide(&s, &preset(), true, &mut fake()), skip);
        let mut f = fake();
        f.delete = DeleteChoice::Skip;
        assert_eq!(decide(&s, &preset(), false, &mut f), skip);
        f.delete = DeleteChoice::Abort;
        assert_eq!(decide(&s, &preset(), false, &mut f), Decision::Abort);
        f.delete = DeleteChoice::Delete;
        assert_eq!(decide(&s, &preset(), false, &mut f), GO);
    }

    #[test]
    fn low_space() {
        let mut s = summary();
        s.insufficient_space = true;
        assert_eq!(decide(&s, &preset(), true, &mut fake()), Decision::Abort);
        assert_eq!(decide(&s, &preset(), false, &mut fake()), GO);
        let mut no = fake();
        no.space = false;
        assert_eq!(decide(&s, &preset(), false, &mut no), Decision::Abort);
    }
}
