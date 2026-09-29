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
    let (edit, original) = match crate::state::begin_edit(path) {
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
    let (var, editor) = ["VISUAL", "EDITOR"]
        .into_iter()
        .find_map(|v| {
            std::env::var(v)
                .ok()
                .filter(|s| !s.trim().is_empty())
                .map(|s| (v, s))
        })
        .unwrap_or(("EDITOR", "vi".into()));
    let argv = match split_editor(&editor, &config::home_dir()) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("✗ ${var} {editor:?}: {e}");
            let _ = crate::state::discard_edit(&edit);
            return 2;
        }
    };
    loop {
        let status = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .arg(&edit)
            .status();
        if let Err(e) = status {
            eprintln!("✗ cannot start {}: {e}", argv[0]);
            let _ = crate::state::discard_edit(&edit);
            return 2;
        }
        let text = std::fs::read_to_string(&edit).unwrap_or_default();
        match Config::parse(&text, path, &config::home_dir()) {
            Ok(c) => {
                match crate::state::commit_edit(&edit, path, &original) {
                    Ok(true) => {}
                    Ok(false) => {
                        eprintln!(
                            "✗ {} changed while you were editing (another `bupr new` or `bupr edit`). \
                             Your version is kept in {}; merge it and run `bupr edit` again.",
                            path.display(),
                            edit.display()
                        );
                        return 2;
                    }
                    Err(e) => {
                        eprintln!("✗ cannot save {}: {e}", path.display());
                        return 2;
                    }
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

/// Split `$VISUAL`/`$EDITOR` into argv words without a shell: whitespace
/// separates, quotes group, a backslash escapes and a leading `~/` is home.
/// Anything that would need a real shell (expansion, operators, globs) is an
/// error rather than being passed on.
pub fn split_editor(value: &str, home: &Path) -> Result<Vec<String>, String> {
    if value.contains('\n') {
        return Err("a newline is not supported".into());
    }
    let mut words = Vec::new();
    let mut word: Option<String> = None;
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        if c == ' ' || c == '\t' {
            words.extend(word.take());
            continue;
        }
        let starts_word = word.is_none();
        let w = word.get_or_insert_with(String::new);
        match c {
            '~' if starts_word && matches!(chars.peek(), Some('/')) => {
                w.push_str(&home.to_string_lossy());
            }
            '\\' => w.push(chars.next().ok_or("ends with a lone backslash")?),
            '\'' => loop {
                match chars.next() {
                    Some('\'') => break,
                    Some(ch) => w.push(ch),
                    None => return Err("has an unterminated ' quote".into()),
                }
            },
            '"' => loop {
                match chars.next() {
                    Some('"') => break,
                    Some('$' | '`') => {
                        return Err("expansion ($ or `) inside quotes is not supported".into());
                    }
                    Some('\\') if matches!(chars.peek(), Some('"' | '\\')) => {
                        w.extend(chars.next());
                    }
                    Some(ch) => w.push(ch),
                    None => return Err("has an unterminated \" quote".into()),
                }
            },
            '$' | '`' | ';' | '&' | '|' | '<' | '>' | '(' | ')' | '{' | '}' | '*' | '?' | '['
            | ']' | '!' | '#' => {
                return Err(format!(
                    "{c:?} needs a shell; bupr runs the editor directly (quote it if it is part of a path)"
                ));
            }
            _ => w.push(c),
        }
    }
    words.extend(word);
    if words.is_empty() {
        return Err("is empty".into());
    }
    Ok(words)
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

#[cfg(test)]
mod tests {
    use super::split_editor;
    use std::path::Path;

    fn split(s: &str) -> Result<Vec<String>, String> {
        split_editor(s, Path::new("/Users/me"))
    }

    fn ok(s: &str) -> Vec<String> {
        split(s).unwrap_or_else(|e| panic!("{s:?}: {e}"))
    }

    #[test]
    fn common_editors_split_into_words() {
        assert_eq!(ok("vim"), ["vim"]);
        assert_eq!(ok("code --wait"), ["code", "--wait"]);
        assert_eq!(ok("  subl\t-w  "), ["subl", "-w"]);
        assert_eq!(
            ok("'/Applications/Sublime Text.app/Contents/SharedSupport/bin/subl' -w"),
            [
                "/Applications/Sublime Text.app/Contents/SharedSupport/bin/subl",
                "-w"
            ]
        );
        assert_eq!(
            ok("\"/Applications/My Editor.app/bin/ed\" --new-window"),
            ["/Applications/My Editor.app/bin/ed", "--new-window"]
        );
        assert_eq!(
            ok(r"/Applications/Sublime\ Text.app/bin/subl"),
            ["/Applications/Sublime Text.app/bin/subl"]
        );
        assert_eq!(ok("emacsclient -a ''"), ["emacsclient", "-a", ""]);
        assert_eq!(ok("vim -c 'set tw=0'"), ["vim", "-c", "set tw=0"]);
        assert_eq!(ok(r#"ed "a\"b\\c""#), ["ed", r#"a"b\c"#]);
        assert_eq!(ok("'it''s'"), ["its"]);
    }

    #[test]
    fn a_leading_tilde_slash_is_home() {
        assert_eq!(ok("~/bin/ed -x"), ["/Users/me/bin/ed", "-x"]);
        assert_eq!(ok("ed ~/x"), ["ed", "/Users/me/x"]);
        assert_eq!(ok("'~/bin/ed'"), ["~/bin/ed"]);
        assert_eq!(ok("''~/bin/ed"), ["~/bin/ed"]);
        assert_eq!(ok("a~/b"), ["a~/b"]);
        assert_eq!(ok("~user"), ["~user"]);
    }

    #[test]
    fn quoted_metacharacters_are_literal() {
        assert_eq!(ok("'vi; touch x'"), ["vi; touch x"]);
        assert_eq!(ok("\"a|b&c(d)*?[e]!#\""), ["a|b&c(d)*?[e]!#"]);
        assert_eq!(ok(r"vi\;x \$HOME"), ["vi;x", "$HOME"]);
        assert_eq!(ok("'$(touch x)'"), ["$(touch x)"]);
    }

    #[test]
    fn shell_syntax_and_injection_are_refused() {
        for bad in [
            "vi; touch x",
            "vi;touch x",
            "$(touch x)",
            "vi `id`",
            "vi $HOME",
            "${EDITOR}",
            "vi && touch x",
            "vi & touch x",
            "vi | tee x",
            "vi > x",
            "vi < x",
            "(vi)",
            "{ vi; }",
            "vi *",
            "vi ?",
            "vi [ab]",
            "!vi",
            "vi # comment",
            "vi\ntouch x",
            "vi\\\ntouch x",
            "'vi\ntouch x'",
            "\"$(touch x)\"",
            "\"`id`\"",
            "vi \"$HOME\"",
        ] {
            assert!(
                split(bad).is_err(),
                "{bad:?} was accepted: {:?}",
                split(bad)
            );
        }
    }

    #[test]
    fn empty_and_unterminated_values_are_refused() {
        for bad in ["", "   ", "\t", "'vi", "\"vi", "vi 'x", "vi \\"] {
            assert!(split(bad).is_err(), "{bad:?} was accepted");
        }
        assert!(split("vi; x").unwrap_err().contains("';'"));
    }
}
