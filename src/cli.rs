//! Command-line interface (spec §8).

use std::io::IsTerminal;
use std::path::Path;
use std::sync::atomic::Ordering;

use clap::{Parser, Subcommand};

use crate::config::{self, Config};
use crate::engine::Mode;
use crate::preflight::Env;
use crate::runner::{self, Ctx};
use crate::ui::prompt::InquirePrompter;
use crate::worker;

#[derive(Parser, Debug)]
#[command(name = "bupr", version, about = "Preset-based mirror backups")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Cmd>,
    /// Presets to run, in order
    pub presets: Vec<String>,
    /// Run every preset whose destination is available
    #[arg(long, conflicts_with = "presets")]
    pub all: bool,
    /// Show what would happen; change nothing
    #[arg(long, conflicts_with = "simulate")]
    pub dry_run: bool,
    /// Full run with live progress that reads every file but writes nothing
    #[arg(long)]
    pub simulate: bool,
    /// With --dry-run: list every path
    #[arg(short, long)]
    pub verbose: bool,
    /// Never prompt; take the safe choice
    #[arg(short, long)]
    pub yes: bool,
    /// Config file (default: ~/.config/bupr/config.toml)
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<std::path::PathBuf>,
    /// Disable colors
    #[arg(long, global = true)]
    pub no_color: bool,
    /// Only print summaries and errors
    #[arg(short, long, global = true)]
    pub quiet: bool,
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Presets with destination status and last run
    List,
    /// Run history
    Log {
        /// Only this preset
        preset: Option<String>,
        /// How many runs to show
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: usize,
    },
    /// Write a starter config
    Init,
    /// Open the config in $VISUAL/$EDITOR and validate it on save
    Edit,
    /// Show the built-in rule packs
    Rules,
    /// Create a preset interactively
    New,
    /// What a preset backs up, what it skips and why, with exclude hints
    Audit { preset: String },
    #[command(name = "__worker", hide = true)]
    Worker,
}

pub fn main() -> i32 {
    let cli = match Cli::try_parse() {
        Ok(c) => c,
        Err(e) => {
            let code = if e.use_stderr() { 2 } else { 0 };
            let _ = e.print();
            return code;
        }
    };
    if matches!(cli.command, Some(Cmd::Worker)) {
        return worker::worker_main();
    }
    // First Ctrl-C: the worker (same process group) stops cleanly and the
    // parent stops starting new presets. Second: exit immediately.
    let _ = ctrlc::set_handler(|| {
        if runner::INTERRUPTS.fetch_add(1, Ordering::SeqCst) >= 1 {
            std::process::exit(2);
        }
    });
    let config_path = cli
        .config
        .clone()
        .unwrap_or_else(config::default_config_path);
    dispatch(&cli, &config_path)
}

fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

fn dispatch(cli: &Cli, config_path: &Path) -> i32 {
    match &cli.command {
        Some(Cmd::Worker) => unreachable!("handled in main"),
        Some(Cmd::Init) => cmd_init(config_path),
        Some(Cmd::Rules) => cmd_rules(),
        Some(Cmd::New) => cmd_new(cli, config_path),
        Some(Cmd::Audit { preset }) => match load_config(config_path) {
            Ok(c) => match c.get(preset) {
                Some(p) => match crate::audit::audit(p, crate::audit::HINT_MIN_BYTES) {
                    Ok(rep) => {
                        crate::audit::print(p, &rep, &config::home_dir());
                        0
                    }
                    Err(e) => {
                        eprintln!("✗ {e}");
                        2
                    }
                },
                None => {
                    eprintln!("✗ unknown preset {preset:?}");
                    2
                }
            },
            Err(code) => code,
        },
        Some(Cmd::Edit) => cmd_edit(config_path, cli.yes || !interactive()),
        Some(Cmd::Log { preset, limit }) => cmd_log(preset.as_deref(), *limit),
        Some(Cmd::List) => match load_config(config_path) {
            Ok(c) => cmd_list(&context(cli, c)),
            Err(code) => code,
        },
        None => run_command(cli, config_path),
    }
}

fn load_config(path: &Path) -> Result<Config, i32> {
    Config::load(path, &config::home_dir()).map_err(|e| {
        if e.is_missing() {
            eprintln!(
                "No config found at {}.\nRun `bupr init` for a starter config, or `bupr new` to create a preset.",
                path.display()
            );
        } else {
            eprintln!("✗ {e}");
        }
        2
    })
}

fn context(cli: &Cli, config: Config) -> Ctx {
    Ctx {
        config,
        env: Env::system(),
        history_path: config::default_history_path(),
        unattended: cli.yes || !interactive(),
        quiet: cli.quiet,
        color: !cli.no_color
            && std::env::var_os("NO_COLOR").is_none()
            && std::io::stdout().is_terminal(),
        mode: if cli.dry_run {
            Mode::DryRun
        } else if cli.simulate {
            Mode::Simulate
        } else {
            Mode::Run
        },
        verbose: cli.verbose,
    }
}

fn print_presets(ctx: &Ctx) {
    for p in &ctx.config.presets {
        println!(
            "  {:<12} {} → {}",
            p.name,
            crate::ui::format::tilde(&p.source, &ctx.env.home),
            crate::ui::format::tilde(&p.destination, &ctx.env.home)
        );
    }
}

fn run_command(cli: &Cli, config_path: &Path) -> i32 {
    if cli.presets.is_empty() && !cli.all && interactive() && !config_path.exists() {
        println!("No presets yet — let's create one.");
        return cmd_new(cli, config_path);
    }
    let config = match load_config(config_path) {
        Ok(c) => c,
        Err(code) => return code,
    };
    let ctx = context(cli, config);
    let mut prompter = InquirePrompter {
        home: ctx.env.home.clone(),
    };
    if cli.all {
        let (names, skipped) = runner::available_presets(&ctx);
        for (n, why) in &skipped {
            println!("– skipping {n}: {why}");
        }
        if names.is_empty() {
            eprintln!("Nothing to run.");
            return 2;
        }
        return runner::run_presets(&ctx, &names, &mut prompter);
    }
    if cli.presets.is_empty() {
        if !ctx.unattended {
            return run_menu(&ctx, config_path, &mut prompter);
        }
        println!("Presets:");
        print_presets(&ctx);
        eprintln!(
            "Name a preset to run, e.g. `bupr {}`.",
            ctx.config.presets.first().map_or("dev", |p| &p.name)
        );
        return 2;
    }
    runner::run_presets(&ctx, &cli.presets, &mut prompter)
}

fn run_menu(ctx: &Ctx, config_path: &Path, prompter: &mut InquirePrompter) -> i32 {
    let history = crate::history::read_all(&ctx.history_path);
    let rows = crate::ui::menu::rows(&ctx.config, &history, &ctx.env, jiff::Timestamp::now());
    if rows.is_empty() {
        eprintln!("No presets yet. Run `bupr new` to create one.");
        return 2;
    }
    match crate::ui::menu::pick(&rows, true) {
        crate::ui::menu::Choice::Run(name) => runner::run_presets(ctx, &[name], prompter),
        crate::ui::menu::Choice::New => match crate::ui::wizard::run(config_path, &ctx.env) {
            Ok(_) => 0,
            Err(e) => {
                eprintln!("✗ {e}");
                2
            }
        },
        crate::ui::menu::Choice::Quit => 0,
    }
}

fn cmd_list(ctx: &Ctx) -> i32 {
    if ctx.config.presets.is_empty() {
        println!("No presets yet. Run `bupr new` to create one.");
        return 0;
    }
    let history = crate::history::read_all(&ctx.history_path);
    for r in crate::ui::menu::rows(&ctx.config, &history, &ctx.env, jiff::Timestamp::now()) {
        println!("  {}", r.label);
    }
    0
}

fn cmd_log(preset: Option<&str>, limit: usize) -> i32 {
    use crate::engine::Outcome;
    use crate::ui::format::{bytes, clock, count};
    let records: Vec<_> = crate::history::read_all(&config::default_history_path())
        .into_iter()
        .filter(|r| preset.is_none_or(|p| r.preset == p))
        .collect();
    if records.is_empty() {
        println!("No runs recorded yet.");
        return 0;
    }
    let tz = jiff::tz::TimeZone::system();
    for r in records.iter().skip(records.len().saturating_sub(limit)) {
        let when = r.started_at().map_or_else(
            || r.started.clone(),
            |t| {
                t.to_zoned(tz.clone())
                    .strftime("%Y-%m-%d %H:%M")
                    .to_string()
            },
        );
        let outcome = match r.outcome {
            Outcome::Ok => "ok".to_string(),
            Outcome::Errors => format!("{} errors", r.errors.len()),
            Outcome::DeletionsSkipped => "deletions skipped".to_string(),
            Outcome::Aborted => "aborted".to_string(),
            Outcome::Interrupted => "interrupted".to_string(),
            Outcome::PreflightFailed => "not run".to_string(),
        };
        let mode = match r.mode {
            Mode::Run => "",
            Mode::DryRun => " (dry run)",
            Mode::Simulate => " (simulated)",
        };
        println!(
            "{when}  {:<10} {:<18} {} files ({}), {} deleted  {}{mode}",
            r.preset,
            outcome,
            count(r.copied_files),
            bytes(r.copied_bytes),
            count(r.deleted),
            clock(r.duration_ms / 1000)
        );
        if let Some(m) = &r.message {
            println!("                  {m}");
        }
    }
    0
}

fn cmd_rules() -> i32 {
    use crate::rules::{RULES, RulePack};
    for pack in RulePack::ALL {
        println!("{} — {}", pack.name(), pack.description());
        for r in RULES.iter().filter(|r| r.pack == pack) {
            println!("    {}", r.label());
        }
        println!();
    }
    println!(
        "Never excluded by rules: .git/, lockfiles, .env*, CLAUDE.md, .claude/, docs/, dist/."
    );
    println!(
        "Add your own with `exclude = [...]` and override with `include = [...]` in a preset."
    );
    0
}

fn cmd_init(path: &Path) -> i32 {
    match crate::state::write_new_config(path, config::STARTER_CONFIG) {
        Ok(()) => {
            println!("✓ Wrote a starter config to {}.", path.display());
            println!("  Review it with `bupr edit`, then try `bupr dev --dry-run`.");
            0
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            eprintln!(
                "✗ {} already exists; use `bupr edit` to change it.",
                path.display()
            );
            2
        }
        Err(e) => {
            eprintln!("✗ cannot write {}: {e}", path.display());
            2
        }
    }
}

fn cmd_edit(path: &Path, unattended: bool) -> i32 {
    let edit = match crate::state::begin_edit(path) {
        Ok(p) => p,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!(
                "No config at {} yet. Run `bupr init` first.",
                path.display()
            );
            return 2;
        }
        Err(e) => {
            eprintln!("✗ cannot prepare {}: {e}", path.display());
            return 2;
        }
    };
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".into());
    loop {
        let status = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("{editor} \"$1\""))
            .arg("sh")
            .arg(&edit)
            .status();
        if let Err(e) = status {
            eprintln!("✗ cannot start {editor}: {e}");
            let _ = crate::state::discard_edit(&edit);
            return 2;
        }
        let text = std::fs::read_to_string(&edit).unwrap_or_default();
        match Config::parse(&text, path, &config::home_dir()) {
            Ok(c) => {
                if let Err(e) = crate::state::commit_edit(&edit, path) {
                    eprintln!("✗ cannot save {}: {e}", path.display());
                    return 2;
                }
                println!("✓ Saved {} ({} presets).", path.display(), c.presets.len());
                return 0;
            }
            Err(e) => {
                eprintln!("✗ {e}");
                let again = !unattended
                    && inquire::Select::new(
                        "The config is not valid.",
                        vec!["Re-open the editor", "Discard my changes"],
                    )
                    .prompt()
                    .is_ok_and(|a| a == "Re-open the editor");
                if !again {
                    let _ = crate::state::discard_edit(&edit);
                    eprintln!("Changes discarded; {} is unchanged.", path.display());
                    return 2;
                }
            }
        }
    }
}

fn cmd_new(cli: &Cli, config_path: &Path) -> i32 {
    if !interactive() {
        eprintln!(
            "`bupr new` needs an interactive terminal; edit the config with `bupr edit` instead."
        );
        return 2;
    }
    let env = Env::system();
    match crate::ui::wizard::run(config_path, &env) {
        Ok(Some(name)) => {
            let dry = inquire::Confirm::new("Do a dry run now?")
                .with_default(true)
                .prompt()
                .unwrap_or(false);
            if !dry {
                return 0;
            }
            match load_config(config_path) {
                Ok(c) => {
                    let mut ctx = context(cli, c);
                    ctx.mode = Mode::DryRun;
                    runner::run_presets(
                        &ctx,
                        &[name],
                        &mut InquirePrompter {
                            home: ctx.env.home.clone(),
                        },
                    )
                }
                Err(code) => code,
            }
        }
        Ok(None) => 0,
        Err(e) => {
            eprintln!("✗ {e}");
            2
        }
    }
}
