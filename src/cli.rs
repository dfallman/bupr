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
#[command(
    name = "bupr",
    version,
    about = "Preset-based mirror backups",
    args_conflicts_with_subcommands = true
)]
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
