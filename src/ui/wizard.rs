//! `bupr new`: create a preset interactively (spec §8.3).

use std::fs;
use std::path::{Path, PathBuf};

use inquire::autocompletion::{Autocomplete, Replacement};
use inquire::validator::Validation;
use inquire::{Confirm, CustomUserError, MultiSelect, Text};

use crate::config::{self, Config, Preset};
use crate::preflight::{self, Env, PreflightError};
use crate::rules::{self, RulePack};
use crate::state;
use crate::ui::format::tilde;

pub struct Answers {
    pub name: String,
    pub description: Option<String>,
    pub source: String,
    pub destination: String,
    pub rules: Vec<RulePack>,
    pub exclude: Vec<String>,
    pub allow_internal: bool,
}

fn q(s: &str) -> String {
    toml::Value::String(s.to_string()).to_string()
}

pub fn render_block(a: &Answers) -> String {
    let list = |v: &[String]| v.iter().map(|s| q(s)).collect::<Vec<_>>().join(", ");
    let mut out = format!("\n[presets.{}]\n", a.name);
    if let Some(d) = &a.description {
        out += &format!("description = {}\n", q(d));
    }
    out += &format!("source      = {}\n", q(&a.source));
    out += &format!("destination = {}\n", q(&a.destination));
    let packs: Vec<String> = a.rules.iter().map(|r| r.name().to_string()).collect();
    out += &format!("rules       = [{}]\n", list(&packs));
    if !a.exclude.is_empty() {
        out += &format!("exclude     = [{}]\n", list(&a.exclude));
    }
    if a.allow_internal {
        out += "allow_internal = true\n";
    }
    out
}

pub fn validate_new_name(name: &str, existing: &[String]) -> Result<(), String> {
    config::validate_name(name)?;
    if existing.iter().any(|e| e == name) {
        return Err(format!("a preset called {name:?} already exists"));
    }
    Ok(())
}

pub fn validate_source(input: &str, home: &Path) -> Result<(), String> {
    let p = config::expand_tilde(input.trim(), home);
    if !p.is_absolute() {
        return Err("use an absolute path or one starting with ~/".into());
    }
    if !p.is_dir() {
        return Err(format!("{} is not a folder", p.display()));
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
pub enum DestCheck {
    Ok,
    NeedsInternal,
}

pub fn check_destination(
    dest: &str,
    source: &str,
    home: &Path,
    env: &Env,
) -> Result<DestCheck, String> {
    let d = config::expand_tilde(dest.trim(), home);
    if !d.is_absolute() {
        return Err("use an absolute path or one starting with ~/".into());
    }
    let p = Preset::minimal("new", config::expand_tilde(source.trim(), home), d);
    match preflight::check_paths(&p, env) {
        Ok(_) => Ok(DestCheck::Ok),
        Err(PreflightError::Internal(_)) => Ok(DestCheck::NeedsInternal),
        Err(e) => Err(e.to_string()),
    }
}

pub fn parse_excludes(s: &str) -> Result<Vec<String>, String> {
    let v: Vec<String> = s
        .split(',')
        .map(|x| x.trim().to_string())
        .filter(|x| !x.is_empty())
        .collect();
    for p in &v {
        rules::validate_pattern(p)?;
    }
    Ok(v)
}

#[derive(Clone)]
pub struct PathCompleter {
    home: PathBuf,
}

impl PathCompleter {
    pub fn new(home: &Path) -> PathCompleter {
        PathCompleter {
            home: home.to_path_buf(),
        }
    }

    /// Folders matching what has been typed, in the same `~/` or absolute form.
    pub fn suggestions(&self, input: &str) -> Vec<String> {
        if input == "~" {
            return vec!["~/".into()];
        }
        let Some(i) = input.rfind('/') else {
            return Vec::new();
        };
        let (dir_part, prefix) = (&input[..=i], &input[i + 1..]);
        let dir = config::expand_tilde(dir_part, &self.home);
        let Ok(rd) = fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut names: Vec<String> = rd
            .flatten()
            .filter(|e| e.path().is_dir())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.starts_with(prefix) && (prefix.starts_with('.') || !n.starts_with('.')))
            .collect();
        names.sort();
        names.truncate(20);
        names
            .into_iter()
            .map(|n| format!("{dir_part}{n}/"))
            .collect()
    }
}

impl Autocomplete for PathCompleter {
    fn get_suggestions(&mut self, input: &str) -> Result<Vec<String>, CustomUserError> {
        Ok(self.suggestions(input))
    }

    fn get_completion(
        &mut self,
        input: &str,
        highlighted: Option<String>,
    ) -> Result<Replacement, CustomUserError> {
        if highlighted.is_some() {
            return Ok(highlighted);
        }
        let s = self.suggestions(input);
        let Some(first) = s.first() else {
            return Ok(None);
        };
        let common = s.iter().fold(first.clone(), |acc, x| {
            acc.chars()
                .zip(x.chars())
                .take_while(|(a, b)| a == b)
                .map(|(a, _)| a)
                .collect()
        });
        Ok((common.len() > input.len()).then_some(common))
    }
}

/// Interactive flow. Returns the new preset's name, or `None` if cancelled.
pub fn run(config_path: &Path, env: &Env) -> anyhow::Result<Option<String>> {
    let home = env.home.clone();
    let existing_text = fs::read_to_string(config_path).unwrap_or_default();
    let existing: Vec<String> = Config::parse(&existing_text, config_path, &home)
        .map(|c| c.presets.into_iter().map(|p| p.name).collect())
        .unwrap_or_default();

    let names = existing.clone();
    let name = Text::new("Preset name:")
        .with_help_message("lowercase, e.g. photos")
        .with_validator(move |s: &str| {
            Ok(match validate_new_name(s.trim(), &names) {
                Ok(()) => Validation::Valid,
                Err(e) => Validation::Invalid(e.into()),
            })
        })
        .prompt()?
        .trim()
        .to_string();
    let description = Text::new("Description (optional):").prompt()?;
    let description = (!description.trim().is_empty()).then(|| description.trim().to_string());

    let h = home.clone();
    let source = Text::new("Folder to back up:")
        .with_autocomplete(PathCompleter::new(&home))
        .with_validator(move |s: &str| {
            Ok(match validate_source(s, &h) {
                Ok(()) => Validation::Valid,
                Err(e) => Validation::Invalid(e.into()),
            })
        })
        .prompt()?
        .trim()
        .to_string();

    let (destination, allow_internal) = loop {
        let d = Text::new("Back it up to:")
            .with_help_message("a folder on your backup drive, e.g. /Volumes/Backup/photos")
            .with_autocomplete(PathCompleter::new(&home))
            .prompt()?
            .trim()
            .to_string();
        match check_destination(&d, &source, &home, env) {
            Ok(DestCheck::Ok) => break (d, false),
            Ok(DestCheck::NeedsInternal) => {
                if Confirm::new("That folder is on this Mac's internal disk, not a backup drive. Use it anyway?")
                    .with_default(false)
                    .prompt()?
                {
                    break (d, true);
                }
            }
            Err(e) => println!("  ✗ {e}"),
        }
    };

    let pack_labels: Vec<String> = RulePack::ALL
        .iter()
        .map(|p| format!("{} — {}", p.name(), p.description()))
        .collect();
    let picked = MultiSelect::new("Skip regenerable files with rule packs:", pack_labels)
        .with_default(&[1])
        .raw_prompt()?;
    let rules: Vec<RulePack> = picked.iter().map(|o| RulePack::ALL[o.index]).collect();

    let exclude = loop {
        let s = Text::new("Extra excludes (comma-separated, optional):")
            .with_help_message("gitignore-style, e.g. /downloads/, *.iso")
            .prompt()?;
        match parse_excludes(&s) {
            Ok(v) => break v,
            Err(e) => println!("  ✗ {e}"),
        }
    };

    let answers = Answers {
        name: name.clone(),
        description,
        source,
        destination,
        rules,
        exclude,
        allow_internal,
    };
    let block = render_block(&answers);
    println!("{block}");
    if !Confirm::new("Add this preset?")
        .with_default(true)
        .prompt()?
    {
        return Ok(None);
    }
    let mut text = existing_text.clone();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&block);
    Config::parse(&text, config_path, &home)?;
    if existing_text.is_empty() && !config_path.exists() {
        state::write_new_config(config_path, &text)?;
    } else {
        state::replace_config(config_path, &text)?;
    }
    println!("✓ Added preset {name} to {}", tilde(config_path, &home));
    Ok(Some(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil as tu;

    fn answers() -> Answers {
        Answers {
            name: "photos".into(),
            description: Some("Lightroom \"catalog\"".into()),
            source: "~/Pictures".into(),
            destination: "/Volumes/Backup/photos".into(),
            rules: vec![RulePack::Junk],
            exclude: vec!["*.tmp".into(), "/cache/".into()],
            allow_internal: false,
        }
    }

    #[test]
    fn rendered_block_parses_back() {
        let text = format!(
            "[presets.dev]\nsource = \"/a\"\ndestination = \"/b\"\n{}",
            render_block(&answers())
        );
        let c = Config::parse(&text, Path::new("c.toml"), Path::new("/Users/me")).unwrap();
        let p = c.get("photos").unwrap();
        assert_eq!(p.description.as_deref(), Some("Lightroom \"catalog\""));
        assert_eq!(p.source, PathBuf::from("/Users/me/Pictures"));
        assert_eq!(p.exclude, vec!["*.tmp".to_string(), "/cache/".to_string()]);
        assert!(!p.allow_internal);
    }

    #[test]
    fn validators() {
        let existing = vec!["dev".to_string()];
        assert!(validate_new_name("photos", &existing).is_ok());
        assert!(
            validate_new_name("dev", &existing)
                .unwrap_err()
                .contains("already")
        );
        assert!(validate_new_name("List", &existing).is_err());
        let t = tempfile::tempdir().unwrap();
        let home = t.path().canonicalize().unwrap();
        tu::mkdir(&home, "Pictures");
        assert!(validate_source("~/Pictures", &home).is_ok());
        assert!(validate_source("~/Nope", &home).is_err());
        assert_eq!(
            parse_excludes(" *.tmp , /cache/ ,").unwrap(),
            vec!["*.tmp", "/cache/"]
        );
        assert!(parse_excludes("a[").is_err());
        assert!(parse_excludes("").unwrap().is_empty());
    }

    #[test]
    fn destination_checks() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().canonicalize().unwrap();
        tu::mkdir(&root, "home/src");
        tu::mkdir(&root, "Volumes");
        let env = Env {
            home: root.join("home"),
            volumes: root.join("Volumes"),
        };
        let home = env.home.clone();
        let src = "~/src";
        assert!(matches!(
            check_destination("~/backup", src, &home, &env),
            Ok(DestCheck::NeedsInternal)
        ));
        assert!(
            check_destination("~/src/inner", src, &home, &env)
                .unwrap_err()
                .contains("overlaps")
        );
        let v = root.join("Volumes/Nope/x");
        assert!(
            check_destination(v.to_str().unwrap(), src, &home, &env)
                .unwrap_err()
                .contains("not mounted")
        );
    }

    #[test]
    fn path_completion_lists_matching_folders() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().canonicalize().unwrap();
        for d in ["dev", "docs", ".hidden", "zeta"] {
            tu::mkdir(&home, d);
        }
        tu::write(&home, "dfile", b"x");
        let c = PathCompleter::new(&home);
        assert_eq!(c.suggestions("~/d"), vec!["~/dev/", "~/docs/"]);
        assert_eq!(c.suggestions("~/."), vec!["~/.hidden/"]);
        assert_eq!(c.suggestions("~"), vec!["~/"]);
        let abs = format!("{}/z", home.display());
        assert_eq!(
            c.suggestions(&abs),
            vec![format!("{}/zeta/", home.display())]
        );
    }
}
