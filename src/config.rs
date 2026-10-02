//! Preset configuration (spec §4).

use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::preflight;
use crate::rules::{Filter, RulePack};

pub const RESERVED_NAMES: &[&str] = &[
    "audit", "list", "log", "new", "edit", "init", "rules", "help",
];
pub const DEFAULT_MAX_DELETE: u64 = 200;
pub const DEFAULT_MAX_DELETE_SIZE: &str = "10 GB";

pub const STARTER_CONFIG: &str = r#"# bupr presets — https://github.com/dfallman/bupr
# `bupr rules` lists what each rule pack skips; `bupr audit dev` suggests more.

[presets.dev]
description = "All my code"
source      = "~/dev"
destination = "/Volumes/Backup/dev"
rules       = ["dev"]   # skip target/, node_modules/, .build/ and friends
exclude     = [
  # "/some-project/recordings/",   # gitignore-style; a leading / is relative to source
]

# Another preset, run with `bupr photos` (or `bupr --all`):
#
# [presets.photos]
# source      = "~/Pictures"
# destination = "/Volumes/Backup/photos"
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

/// What to do with online-only (dataless) files from iCloud Drive or
/// Dropbox, whose data is not on this Mac.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OnlineOnly {
    /// Leave them online; new or changed ones are not backed up.
    #[default]
    Skip,
    /// Let macOS download them, which keeps them downloaded on this Mac.
    Download,
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
    #[serde(default)]
    secrets_require_encryption: bool,
    #[serde(default)]
    online_only: OnlineOnly,
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
    /// Ask (or, unattended, abort) before copying secret files to a drive
    /// that is not known to be encrypted (AUD-M5).
    pub secrets_require_encryption: bool,
    pub online_only: OnlineOnly,
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
            secrets_require_encryption: false,
            online_only: OnlineOnly::Skip,
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
            secrets_require_encryption: raw.secrets_require_encryption,
            online_only: raw.online_only,
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
        // A mirror deletes everything it does not own, so two presets must
        // never share or nest destinations, however their paths are spelled.
        let keys: Vec<String> = presets
            .iter()
            .map(|p| preflight::dest_key(&p.destination))
            .collect();
        for (i, b) in presets.iter().enumerate() {
            if let Some(j) = (0..i).find(|&j| preflight::keys_overlap(&keys[i], &keys[j])) {
                return Err(overlap_error(b, &presets[j]));
            }
        }
        Ok(Config { presets })
    }

    /// Another preset whose destination is, or holds, or lies inside this
    /// one's, checked again at the start of a run (AUD-H6).
    pub fn check_overlap(&self, preset: &Preset) -> Result<(), ConfigError> {
        let key = preflight::dest_key(&preset.destination);
        match self.presets.iter().find(|o| {
            o.name != preset.name
                && preflight::keys_overlap(&key, &preflight::dest_key(&o.destination))
        }) {
            Some(o) => Err(overlap_error(preset, o)),
            None => Ok(()),
        }
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

fn overlap_error(b: &Preset, a: &Preset) -> ConfigError {
    ConfigError::Invalid {
        preset: b.name.clone(),
        message: format!(
            "destination {} overlaps the destination of preset \"{}\" ({})",
            b.destination.display(),
            a.name,
            a.destination.display()
        ),
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
        assert_eq!(p.online_only, OnlineOnly::Skip);
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
        let dl =
            parse("[presets.d]\nsource=\"/a\"\ndestination=\"/b\"\nonline_only=\"download\"\n")
                .unwrap();
        assert_eq!(dl.get("d").unwrap().online_only, OnlineOnly::Download);
        assert!(
            parse("[presets.d]\nsource=\"/a\"\ndestination=\"/b\"\nonline_only=\"yes\"\n").is_err()
        );
        let media = parse(
            "[presets.d]\nsource=\"/a\"\ndestination=\"/b\"\nrules=[\"junk\", \"video\", \"music\"]\n",
        )
        .unwrap();
        assert_eq!(
            media.get("d").unwrap().rules,
            vec![RulePack::Junk, RulePack::Video, RulePack::Music]
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
    fn starter_config_parses_and_is_generic() {
        let c = parse(STARTER_CONFIG).unwrap();
        assert_eq!(c.presets.len(), 1);
        let dev = c.get("dev").unwrap();
        assert_eq!(dev.rules, vec![RulePack::Dev]);
        assert_eq!(dev.source, PathBuf::from("/Users/me/dev"));
        assert_eq!(dev.destination, PathBuf::from("/Volumes/Backup/dev"));
        for personal in ["video", "archive", "Externals"] {
            assert!(!STARTER_CONFIG.contains(personal), "{personal}");
        }
        // The commented-out example preset must parse once uncommented.
        let uncommented: String = STARTER_CONFIG
            .lines()
            .map(|l| {
                l.strip_prefix("# ")
                    .filter(|r| r.starts_with('[') || r.contains(" = "))
                    .unwrap_or(l)
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(parse(&uncommented).unwrap().get("photos").is_some());
    }

    #[test]
    fn overlapping_destinations_across_presets_are_rejected() {
        let base = "[presets.dev]\nsource=\"/a\"\ndestination=\"/Volumes/B/dev\"\n";
        for other in ["/Volumes/B/dev", "/Volumes/B/dev/media", "/Volumes/B"] {
            let text = format!("{base}[presets.media]\nsource=\"/m\"\ndestination=\"{other}\"\n");
            let e = parse(&text).unwrap_err();
            assert!(e.to_string().contains("overlaps"), "{other}: {e}");
        }
        let ok =
            format!("{base}[presets.media]\nsource=\"/m\"\ndestination=\"/Volumes/B/devices\"\n");
        assert!(parse(&ok).is_ok());
    }

    #[test]
    fn destinations_that_are_one_folder_by_another_name_are_rejected() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().canonicalize().unwrap();
        crate::testutil::mkdir(&root, "real");
        crate::testutil::symlink(root.join("real").to_str().unwrap(), &root, "link");
        let mut others = vec![root.join("link/dev").display().to_string()];
        if preflight::is_case_insensitive(&root) {
            others.push(root.join("REAL/Dev").display().to_string());
        }
        for other in others {
            let text = format!(
                "[presets.dev]\nsource=\"/a\"\ndestination={:?}\n\
                 [presets.media]\nsource=\"/m\"\ndestination={other:?}\n",
                root.join("real/dev").display().to_string()
            );
            let e = parse(&text).unwrap_err();
            assert!(e.to_string().contains("overlaps"), "{other}: {e}");
        }
        let c = parse("[presets.dev]\nsource=\"/a\"\ndestination=\"/Volumes/B/dev\"\n").unwrap();
        let mut late = Preset::minimal("media", "/m".into(), "/Volumes/B/dev/m".into());
        assert!(c.check_overlap(&late).is_err());
        late.destination = "/Volumes/B/media".into();
        assert!(c.check_overlap(&late).is_ok());
        assert!(c.check_overlap(c.get("dev").unwrap()).is_ok());
    }

    #[test]
    fn empty_config_has_no_presets() {
        assert!(parse("").unwrap().presets.is_empty());
    }
}
