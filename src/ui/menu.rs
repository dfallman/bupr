//! The preset picker shown by a bare `bupr` (spec §8.1).

use inquire::Select;

use crate::config::Config;
use crate::history::{self, RunRecord};
use crate::menu_reason;
use crate::preflight::{self, Env};
use crate::ui::format::{relative, tilde};

pub const NEW_LABEL: &str = "+ New preset";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub name: String,
    pub label: String,
    pub available: bool,
    pub reason: Option<String>,
}

pub fn rows(config: &Config, history: &[RunRecord], env: &Env, now: jiff::Timestamp) -> Vec<Row> {
    let w = config
        .presets
        .iter()
        .map(|p| p.name.chars().count())
        .max()
        .unwrap_or(0);
    config
        .presets
        .iter()
        .map(|p| {
            let route = format!(
                "{} → {}",
                tilde(&p.source, &env.home),
                tilde(&p.destination, &env.home)
            );
            match preflight::check_paths(p, env) {
                Ok(_) => {
                    let last = history::last_real_run(history, &p.name)
                        .and_then(|r| r.started_at())
                        .map_or_else(
                            || "never".to_string(),
                            |t| relative(now.as_second(), t.as_second()),
                        );
                    Row {
                        name: p.name.clone(),
                        label: format!("{:<w$}  {route}  {last}", p.name),
                        available: true,
                        reason: None,
                    }
                }
                Err(e) => Row {
                    name: p.name.clone(),
                    label: format!("{:<w$}  {route}  ({})", p.name, menu_reason(&e)),
                    available: false,
                    reason: Some(e.to_string()),
                },
            }
        })
        .collect()
}

pub enum Choice {
    Run(String),
    New,
    Quit,
}

pub fn pick(rows: &[Row], offer_new: bool) -> Choice {
    let mut options: Vec<String> = rows.iter().map(|r| r.label.clone()).collect();
    if offer_new {
        options.push(NEW_LABEL.to_string());
    }
    let mut cursor = 0;
    loop {
        let answer = Select::new("Pick a preset", options.clone())
            .with_help_message("↑↓ move · enter run · type to filter · esc quit")
            .with_starting_cursor(cursor)
            .with_page_size(12)
            .raw_prompt_skippable();
        match answer {
            Ok(Some(opt)) if opt.index == rows.len() => return Choice::New,
            Ok(Some(opt)) => {
                let r = &rows[opt.index];
                if r.available {
                    return Choice::Run(r.name.clone());
                }
                println!(
                    "  {} can't run: {}",
                    r.name,
                    r.reason.as_deref().unwrap_or("unavailable")
                );
                cursor = opt.index;
            }
            Ok(None) | Err(_) => return Choice::Quit,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Preset;
    use crate::engine::{Mode, Outcome, RunStats};
    use crate::testutil as tu;

    #[test]
    fn rows_show_route_last_run_and_availability() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().canonicalize().unwrap();
        for d in ["home/dev", "Volumes", "backup"] {
            tu::mkdir(&root, d);
        }
        let env = Env {
            home: root.join("home"),
            volumes: root.join("Volumes"),
        };
        let mut dev = Preset::minimal("dev", root.join("home/dev"), root.join("backup/dev"));
        dev.allow_internal = true;
        let media = Preset::minimal("media", root.join("home/dev"), root.join("Volumes/Gone/m"));
        let config = Config {
            presets: vec![dev, media],
        };
        let now: jiff::Timestamp = "2026-09-29T12:00:00Z".parse().unwrap();
        let mut stats = RunStats::new(Mode::Run);
        stats.outcome = Outcome::Ok;
        let then: jiff::Timestamp = "2026-09-27T12:00:00Z".parse().unwrap();
        let history = vec![RunRecord::new("dev", then, then, false, &stats)];

        let r = rows(&config, &history, &env, now);
        assert!(r[0].available);
        assert!(r[0].label.starts_with("dev    ~/dev → "), "{}", r[0].label);
        assert!(r[0].label.ends_with("2 days ago"), "{}", r[0].label);
        assert!(!r[1].available);
        assert!(
            r[1].label.ends_with("(drive not mounted)"),
            "{}",
            r[1].label
        );
        assert!(r[1].reason.as_deref().unwrap().contains("not available"));
    }
}
