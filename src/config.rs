//! Preset configuration (spec §4).

use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::rules::{Filter, RulePack};

pub const RESERVED_NAMES: &[&str] = &[
    "audit", "list", "log", "new", "edit", "init", "rules", "help",
];
pub const DEFAULT_MAX_DELETE: u64 = 200;
pub const DEFAULT_MAX_DELETE_SIZE: &str = "10 GB";

pub const STARTER_CONFIG: &str = r#"# bupr presets — see `bupr rules` for what each rule pack skips.

[presets.dev]
description = "All my code"
source      = "~/dev"
destination = "/Volumes/Backup/dev"
rules       = ["dev"]
exclude     = [
  "/recorder/downloads/",      # recordings — backed up by the media preset
  "/recorder/Drive/",          # stray copies of other projects
  "/archive/test-data/",
  "**/gen/apple/Externals/",  # Tauri-built iOS libraries (regenerable)
]

[presets.media]
description = "recorder recordings"
source      = "~/dev/recorder/downloads"
destination = "/Volumes/Backup/media/video"
"#;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read config {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("config {path}: {message}")]
    Parse { path: PathBuf, message: String },
    #[error("preset \"{preset}\": {message}")]
    Invalid { preset: String, message: String },
}

impl ConfigError {
    pub fn is_missing(&self) -> bool {
        matches!(self, ConfigError::Read { source, .. } if source.kind() == std::io::ErrorKind::NotFound)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    presets: IndexMap<String, RawPreset>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPreset {
    description: Option<String>,
    source: String,
    destination: String,
    rules: Option<Vec<RulePack>>,
    #[serde(default)]
    exclude: Vec<String>,
    #[serde(default)]
    include: Vec<String>,
    max_delete: Option<u64>,
    max_delete_size: Option<String>,
    #[serde(default)]
    secrets: Vec<String>,
    #[serde(default)]
    allow_internal: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Preset {
    pub name: String,
    pub description: Option<String>,
    pub source: PathBuf,
    pub destination: PathBuf,
    pub rules: Vec<RulePack>,
    pub exclude: Vec<String>,
    pub include: Vec<String>,
    pub max_delete: u64,
    pub max_delete_bytes: u64,
    pub secrets: Vec<String>,
    pub allow_internal: bool,
}

impl Preset {
    pub fn minimal(name: &str, source: PathBuf, destination: PathBuf) -> Preset {
        Preset {
            name: name.to_string(),
            description: None,
            source,
            destination,
            rules: vec![RulePack::Junk],
            exclude: Vec::new(),
            include: Vec::new(),
            max_delete: DEFAULT_MAX_DELETE,
            max_delete_bytes: parse_size(DEFAULT_MAX_DELETE_SIZE).expect("default size parses"),
            secrets: Vec::new(),
            allow_internal: false,
        }
    }

    pub fn filter(&self) -> Result<Filter, String> {
        Filter::new(&self.rules, &self.include, &self.exclude, &self.secrets)
    }

    fn from_raw(name: String, raw: RawPreset, home: &Path) -> Result<Preset, ConfigError> {
        let invalid = |message: String| ConfigError::Invalid {
            preset: name.clone(),
            message,
        };
        validate_name(&name).map_err(invalid)?;
        let source = expand_tilde(&raw.source, home);
        if !source.is_absolute() {
            return Err(invalid(format!(
                "source {:?} must be absolute or start with ~/",
                raw.source
            )));
        }
        let destination = expand_tilde(&raw.destination, home);
        if !destination.is_absolute() {
            return Err(invalid(format!(
                "destination {:?} must be absolute or start with ~/",
                raw.destination
            )));
        }
        let max_delete_bytes = parse_size(
            raw.max_delete_size
                .as_deref()
                .unwrap_or(DEFAULT_MAX_DELETE_SIZE),
        )
        .map_err(invalid)?;
        let preset = Preset {
            name: name.clone(),
            description: raw.description,
            source,
            destination,
            rules: raw.rules.unwrap_or_else(|| vec![RulePack::Junk]),
            exclude: raw.exclude,
            include: raw.include,
            max_delete: raw.max_delete.unwrap_or(DEFAULT_MAX_DELETE),
            max_delete_bytes,
            secrets: raw.secrets,
            allow_internal: raw.allow_internal,
        };
        preset.filter().map_err(invalid)?;
        Ok(preset)
    }
}

#[derive(Clone, Debug, Default)]
pub struct Config {
    pub presets: Vec<Preset>,
}

impl Config {
    pub fn parse(text: &str, path: &Path, home: &Path) -> Result<Config, ConfigError> {
        let raw: RawConfig = toml::from_str(text).map_err(|e| ConfigError::Parse {
            path: path.to_path_buf(),
            message: e.to_string(),
        })?;
        let presets = raw
            .presets
            .into_iter()
            .map(|(name, rp)| Preset::from_raw(name, rp, home))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Config { presets })
    }

    pub fn load(path: &Path, home: &Path) -> Result<Config, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(&text, path, home)
    }

    pub fn get(&self, name: &str) -> Option<&Preset> {
        self.presets.iter().find(|p| p.name == name)
    }
}

pub fn validate_name(name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let first_ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    let rest_ok =
        chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if !first_ok || !rest_ok {
        return Err(format!(
            "invalid name {name:?}: use lowercase letters, digits, '-' and '_', starting with a letter or digit"
        ));
    }
    if RESERVED_NAMES.contains(&name) {
        return Err(format!("invalid name {name:?}: it is a bupr command"));
    }
    Ok(())
}

pub fn expand_tilde(s: &str, home: &Path) -> PathBuf {
    if s == "~" {
        home.to_path_buf()
    } else if let Some(rest) = s.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(s)
    }
}

pub fn parse_size(s: &str) -> Result<u64, String> {
    s.trim()
        .parse::<bytesize::ByteSize>()
        .map(|b| b.as_u64())
        .map_err(|e| format!("invalid size {s:?}: {e}"))
}

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn xdg_dir(var: &str, fallback: &str) -> PathBuf {
    match std::env::var_os(var) {
        Some(v) if Path::new(&v).is_absolute() => PathBuf::from(v),
        _ => home_dir().join(fallback),
    }
}

pub fn default_config_path() -> PathBuf {
    xdg_dir("XDG_CONFIG_HOME", ".config")
        .join("bupr")
        .join("config.toml")
}

pub fn default_history_path() -> PathBuf {
    xdg_dir("XDG_STATE_HOME", ".local/state")
        .join("bupr")
        .join("history.jsonl")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Config, ConfigError> {
        Config::parse(text, Path::new("/cfg/config.toml"), Path::new("/Users/me"))
    }

    #[test]
    fn minimal_preset_gets_defaults() {
        let c =
            parse("[presets.dev]\nsource = \"~/dev\"\ndestination = \"/Volumes/B/dev\"\n").unwrap();
        let p = c.get("dev").unwrap();
        assert_eq!(p.source, PathBuf::from("/Users/me/dev"));
        assert_eq!(p.destination, PathBuf::from("/Volumes/B/dev"));
        assert_eq!(p.rules, vec![RulePack::Junk]);
        assert_eq!(p.max_delete, 200);
        assert_eq!(p.max_delete_bytes, 10_000_000_000);
        assert!(!p.allow_internal);
        assert!(p.exclude.is_empty() && p.include.is_empty() && p.description.is_none());
    }

    #[test]
    fn preset_order_is_file_order() {
        let c = parse(
            "[presets.zeta]\nsource=\"/a\"\ndestination=\"/Volumes/B/z\"\n\
             [presets.alpha]\nsource=\"/a\"\ndestination=\"/Volumes/B/a\"\n",
        )
        .unwrap();
        let names: Vec<_> = c.presets.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["zeta", "alpha"]);
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let e = parse("[presets.dev]\nsource=\"/a\"\ndestination=\"/b\"\nexlude=[]\n").unwrap_err();
        assert!(e.to_string().contains("exlude"), "{e}");
        assert!(parse("[preset.dev]\nsource=\"/a\"\ndestination=\"/b\"\n").is_err());
    }

    #[test]
    fn names_are_validated() {
        assert!(validate_name("dev").is_ok());
        assert!(validate_name("media-2_x").is_ok());
        for bad in ["", "Dev", "-x", "a b", "list", "help", "new"] {
            assert!(validate_name(bad).is_err(), "{bad}");
        }
        let e = parse("[presets.list]\nsource=\"/a\"\ndestination=\"/b\"\n").unwrap_err();
        assert!(e.to_string().contains("preset \"list\""), "{e}");
    }

    #[test]
    fn paths_must_be_absolute() {
        let e = parse("[presets.dev]\nsource=\"dev\"\ndestination=\"/b\"\n").unwrap_err();
        assert!(e.to_string().contains("source"), "{e}");
        assert!(parse("[presets.dev]\nsource=\"/a\"\ndestination=\"b\"\n").is_err());
    }

    #[test]
    fn sizes_rule_packs_and_globs_are_validated() {
        assert_eq!(parse_size("10 GB").unwrap(), 10_000_000_000);
        assert_eq!(parse_size("500MB").unwrap(), 500_000_000);
        assert!(parse_size("lots").is_err());
        assert!(
            parse("[presets.d]\nsource=\"/a\"\ndestination=\"/b\"\nmax_delete_size=\"x\"\n")
                .is_err()
        );
        assert!(
            parse("[presets.d]\nsource=\"/a\"\ndestination=\"/b\"\nrules=[\"rust\"]\n").is_err()
        );
        assert!(
            parse("[presets.d]\nsource=\"/a\"\ndestination=\"/b\"\nexclude=[\"a[\"]\n").is_err()
        );
    }

    #[test]
    fn tilde_expansion() {
        let h = Path::new("/Users/me");
        assert_eq!(expand_tilde("~", h), PathBuf::from("/Users/me"));
        assert_eq!(expand_tilde("~/x", h), PathBuf::from("/Users/me/x"));
        assert_eq!(expand_tilde("/abs", h), PathBuf::from("/abs"));
        assert_eq!(expand_tilde("~other/x", h), PathBuf::from("~other/x"));
    }

    #[test]
    fn starter_config_parses() {
        let c = parse(STARTER_CONFIG).unwrap();
        let dev = c.get("dev").unwrap();
        assert_eq!(dev.rules, vec![RulePack::Dev]);
        assert!(dev.exclude.contains(&"/recorder/downloads/".to_string()));
        assert_eq!(
            c.get("media").unwrap().destination,
            PathBuf::from("/Volumes/Backup/media/video")
        );
    }

    #[test]
    fn empty_config_has_no_presets() {
        assert!(parse("").unwrap().presets.is_empty());
    }
}
