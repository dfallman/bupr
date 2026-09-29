//! Built-in rule packs and include/exclude matching (spec §5).
//!
//! Precedence per entry: include glob → included; inside an excluded
//! directory → excluded (inherited); exclude glob → excluded; rule-pack rule
//! (name + sibling marker) → excluded; otherwise included. `.gitignore` is
//! never consulted here.

use globset::{Glob, GlobBuilder, GlobMatcher};
use serde::{Deserialize, Serialize};

use crate::relpath::RelPath;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RulePack {
    Dev,
    Junk,
}

impl RulePack {
    pub const ALL: [RulePack; 2] = [RulePack::Dev, RulePack::Junk];

    pub fn name(self) -> &'static str {
        match self {
            RulePack::Dev => "dev",
            RulePack::Junk => "junk",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            RulePack::Dev => "build output and dependencies of common toolchains (includes junk)",
            RulePack::Junk => "OS clutter: .DS_Store, AppleDouble files, editor swap files",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Applies {
    File,
    Dir,
    Any,
}

pub struct RuleDef {
    pub pack: RulePack,
    /// Glob matched against the entry's file name.
    pub name: &'static str,
    pub applies: Applies,
    /// Globs matched against sibling names; empty means no marker needed.
    pub markers: &'static [&'static str],
}

impl RuleDef {
    pub fn label(&self) -> String {
        let slash = if self.applies == Applies::Dir {
            "/"
        } else {
            ""
        };
        if self.markers.is_empty() {
            format!("{}{}", self.name, slash)
        } else {
            format!(
                "{}{} next to {}",
                self.name,
                slash,
                self.markers.join(" | ")
            )
        }
    }
}

const fn rule(
    pack: RulePack,
    name: &'static str,
    applies: Applies,
    markers: &'static [&'static str],
) -> RuleDef {
    RuleDef {
        pack,
        name,
        applies,
        markers,
    }
}

const NODE: &[&str] = &["package.json"];
const PYTHON: &[&str] = &["pyproject.toml", "requirements.txt", "setup.py"];

pub const RULES: &[RuleDef] = &[
    rule(RulePack::Junk, ".DS_Store", Applies::File, &[]),
    rule(RulePack::Junk, "._*", Applies::Any, &[]),
    rule(RulePack::Junk, ".Spotlight-V100", Applies::Dir, &[]),
    rule(RulePack::Junk, ".Trashes", Applies::Dir, &[]),
    rule(RulePack::Junk, ".fseventsd", Applies::Dir, &[]),
    rule(RulePack::Junk, ".TemporaryItems", Applies::Dir, &[]),
    rule(RulePack::Junk, "Thumbs.db", Applies::File, &[]),
    rule(RulePack::Junk, "*.swp", Applies::File, &[]),
    rule(RulePack::Junk, "*.swo", Applies::File, &[]),
    rule(RulePack::Dev, "target", Applies::Dir, &["Cargo.toml"]),
    rule(RulePack::Dev, "node_modules", Applies::Dir, NODE),
    rule(RulePack::Dev, ".turbo", Applies::Dir, NODE),
    rule(RulePack::Dev, ".parcel-cache", Applies::Dir, NODE),
    rule(RulePack::Dev, "coverage", Applies::Dir, NODE),
    rule(
        RulePack::Dev,
        ".svelte-kit",
        Applies::Dir,
        &["svelte.config.js", "svelte.config.ts"],
    ),
    rule(RulePack::Dev, ".next", Applies::Dir, &["next.config.*"]),
    rule(RulePack::Dev, ".nuxt", Applies::Dir, &["nuxt.config.*"]),
    rule(
        RulePack::Dev,
        ".wrangler",
        Applies::Dir,
        &["wrangler.toml", "wrangler.json", "wrangler.jsonc"],
    ),
    rule(RulePack::Dev, ".build", Applies::Dir, &["Package.swift"]),
    rule(
        RulePack::Dev,
        "build",
        Applies::Dir,
        &[
            "*.xcodeproj",
            "*.xcworkspace",
            "build.gradle",
            "build.gradle.kts",
        ],
    ),
    rule(
        RulePack::Dev,
        ".gradle",
        Applies::Dir,
        &["build.gradle*", "settings.gradle*"],
    ),
    rule(RulePack::Dev, "Pods", Applies::Dir, &["Podfile"]),
    rule(RulePack::Dev, "vendor", Applies::Dir, &["composer.json"]),
    rule(RulePack::Dev, ".venv", Applies::Dir, PYTHON),
    rule(RulePack::Dev, "venv", Applies::Dir, PYTHON),
    rule(RulePack::Dev, "DerivedData", Applies::Dir, &[]),
    rule(RulePack::Dev, "__pycache__", Applies::Dir, &[]),
    rule(RulePack::Dev, ".pytest_cache", Applies::Dir, &[]),
    rule(RulePack::Dev, ".mypy_cache", Applies::Dir, &[]),
    rule(RulePack::Dev, ".ruff_cache", Applies::Dir, &[]),
];

pub const BUILTIN_SECRETS: &[&str] = &[
    ".env",
    ".env.*",
    "*.pem",
    "*.key",
    "id_rsa*",
    "id_ed25519*",
    "id_ecdsa*",
];

/// Rules active for a preset; `dev` implies `junk`.
pub fn rules_for(packs: &[RulePack]) -> Vec<&'static RuleDef> {
    let dev = packs.contains(&RulePack::Dev);
    let junk = dev || packs.contains(&RulePack::Junk);
    RULES
        .iter()
        .filter(|r| match r.pack {
            RulePack::Dev => dev,
            RulePack::Junk => junk,
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Reason {
    Rule(String),
    Exclude(String),
    Inherited,
}

impl Reason {
    pub fn label(&self) -> String {
        match self {
            Reason::Rule(l) => l.clone(),
            Reason::Exclude(p) => format!("exclude {p}"),
            Reason::Inherited => "inside an excluded folder".to_string(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    Include,
    Exclude(Reason),
}

struct Pattern {
    original: String,
    /// Normalized glob (anchored, or prefixed with `**/`).
    glob: String,
    dir_only: bool,
    matcher: GlobMatcher,
}

impl Pattern {
    fn matches(&self, rel: &RelPath, is_dir: bool) -> bool {
        (!self.dir_only || is_dir) && self.matcher.is_match(rel.as_str())
    }
}

/// Compile a gitignore-style pattern: trailing `/` = directories only; a
/// leading or middle `/` anchors it to the root; otherwise it matches at any
/// depth.
fn compile_pattern(p: &str) -> Result<Pattern, String> {
    let trimmed = p.trim();
    let dir_only = trimmed.ends_with('/');
    let body = trimmed.trim_end_matches('/');
    let anchored = body.contains('/');
    let body = body.trim_start_matches('/');
    if body.is_empty() {
        return Err(format!("invalid pattern {p:?}: empty"));
    }
    let glob = if anchored || body.starts_with("**/") {
        body.to_string()
    } else {
        format!("**/{body}")
    };
    let matcher = GlobBuilder::new(&glob)
        .literal_separator(true)
        .build()
        .map_err(|e| format!("invalid pattern {p:?}: {e}"))?
        .compile_matcher();
    Ok(Pattern {
        original: p.to_string(),
        glob,
        dir_only,
        matcher,
    })
}

pub fn validate_pattern(p: &str) -> Result<(), String> {
    compile_pattern(p).map(|_| ())
}

struct CompiledRule {
    label: String,
    name: GlobMatcher,
    applies: Applies,
    markers: Vec<GlobMatcher>,
}

fn name_glob(g: &str) -> GlobMatcher {
    Glob::new(g)
        .expect("built-in glob is valid")
        .compile_matcher()
}

pub struct Filter {
    include: Vec<Pattern>,
    exclude: Vec<Pattern>,
    rules: Vec<CompiledRule>,
    secrets: Vec<Pattern>,
}

impl Filter {
    pub fn new(
        packs: &[RulePack],
        include: &[String],
        exclude: &[String],
        extra_secrets: &[String],
    ) -> Result<Filter, String> {
        let compile_all = |v: &[String]| {
            v.iter()
                .map(|p| compile_pattern(p))
                .collect::<Result<Vec<_>, _>>()
        };
        let rules = rules_for(packs)
            .into_iter()
            .map(|r| CompiledRule {
                label: r.label(),
                name: name_glob(r.name),
                applies: r.applies,
                markers: r.markers.iter().map(|m| name_glob(m)).collect(),
            })
            .collect();
        let mut secrets: Vec<String> = BUILTIN_SECRETS.iter().map(|s| s.to_string()).collect();
        secrets.extend(extra_secrets.iter().cloned());
        Ok(Filter {
            include: compile_all(include)?,
            exclude: compile_all(exclude)?,
            rules,
            secrets: compile_all(&secrets)?,
        })
    }

    pub fn decide(
        &self,
        rel: &RelPath,
        is_dir: bool,
        siblings: &[String],
        parent_excluded: bool,
    ) -> Decision {
        if self.include.iter().any(|p| p.matches(rel, is_dir)) {
            return Decision::Include;
        }
        if parent_excluded {
            return Decision::Exclude(Reason::Inherited);
        }
        if let Some(p) = self.exclude.iter().find(|p| p.matches(rel, is_dir)) {
            return Decision::Exclude(Reason::Exclude(p.original.clone()));
        }
        let name = rel.file_name();
        for r in &self.rules {
            let kind_ok = match r.applies {
                Applies::File => !is_dir,
                Applies::Dir => is_dir,
                Applies::Any => true,
            };
            if kind_ok
                && r.name.is_match(name)
                && (r.markers.is_empty()
                    || siblings
                        .iter()
                        .any(|s| r.markers.iter().any(|m| m.is_match(s))))
            {
                return Decision::Exclude(Reason::Rule(r.label.clone()));
            }
        }
        Decision::Include
    }

    /// Could some include pattern match an entry at or below `dir`? Used to
    /// decide whether an excluded directory must still be walked (spec §5.1).
    pub fn may_include_below(&self, dir: &RelPath) -> bool {
        let dir_c: Vec<&str> = dir.components().collect();
        self.include.iter().any(|p| {
            let lit: Vec<&str> = p
                .glob
                .split('/')
                .take_while(|c| !c.contains(['*', '?', '[', '{']))
                .collect();
            let n = lit.len().min(dir_c.len());
            lit[..n] == dir_c[..n]
        })
    }

    pub fn is_secret(&self, rel: &RelPath) -> bool {
        self.secrets.iter().any(|p| p.matches(rel, false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }
    fn r(p: &str) -> RelPath {
        RelPath::new(p).unwrap()
    }
    fn filter(packs: &[RulePack], include: &[&str], exclude: &[&str]) -> Filter {
        Filter::new(packs, &s(include), &s(exclude), &[]).unwrap()
    }
    fn excluded(d: Decision) -> bool {
        matches!(d, Decision::Exclude(_))
    }

    #[test]
    fn target_excluded_only_next_to_cargo_toml() {
        let f = filter(&[RulePack::Dev], &[], &[]);
        let d = f.decide(
            &r("tools/target"),
            true,
            &s(&["Cargo.toml", "src", "target"]),
            false,
        );
        assert_eq!(
            d,
            Decision::Exclude(Reason::Rule("target/ next to Cargo.toml".into()))
        );
        assert_eq!(
            f.decide(&r("notes/target"), true, &s(&["target", "todo.md"]), false),
            Decision::Include
        );
        // A *file* called target is never a build dir.
        assert_eq!(
            f.decide(&r("x/target"), false, &s(&["Cargo.toml", "target"]), false),
            Decision::Include
        );
    }

    #[test]
    fn build_dir_needs_a_project_marker() {
        let f = filter(&[RulePack::Dev], &[], &[]);
        assert!(excluded(f.decide(
            &r("x/build"),
            true,
            &s(&["app.xcodeproj", "build"]),
            false
        )));
        assert!(excluded(f.decide(
            &r("x/build"),
            true,
            &s(&["build.gradle.kts", "build"]),
            false
        )));
        assert_eq!(
            f.decide(&r("x/build"), true, &s(&["build", "README.md"]), false),
            Decision::Include
        );
    }

    #[test]
    fn marker_globs_match() {
        let f = filter(&[RulePack::Dev], &[], &[]);
        assert!(excluded(f.decide(
            &r("w/.next"),
            true,
            &s(&["next.config.mjs"]),
            false
        )));
        assert!(excluded(f.decide(
            &r("w/.svelte-kit"),
            true,
            &s(&["svelte.config.js"]),
            false
        )));
    }

    #[test]
    fn junk_is_part_of_dev_and_default() {
        for packs in [&[RulePack::Dev][..], &[RulePack::Junk][..]] {
            let f = filter(packs, &[], &[]);
            assert!(excluded(f.decide(&r("a/.DS_Store"), false, &[], false)));
            assert!(excluded(f.decide(&r("a/._foo"), false, &[], false)));
            assert!(excluded(f.decide(&r("a/x.swp"), false, &[], false)));
        }
        let junk_only = filter(&[RulePack::Junk], &[], &[]);
        assert_eq!(
            junk_only.decide(&r("p/target"), true, &s(&["Cargo.toml"]), false),
            Decision::Include
        );
    }

    #[test]
    fn no_packs_includes_everything() {
        let f = filter(&[], &[], &[]);
        assert_eq!(
            f.decide(&r("a/.DS_Store"), false, &[], false),
            Decision::Include
        );
    }

    #[test]
    fn private_but_important_files_are_never_excluded_by_rules() {
        let f = filter(&[RulePack::Dev], &[], &[]);
        let sib = s(&["Cargo.toml", "package.json", "composer.json"]);
        for (p, dir) in [
            ("p/.git", true),
            ("p/.claude", true),
            ("p/docs", true),
            ("p/CLAUDE.md", false),
            ("p/.env", false),
            ("p/Cargo.lock", false),
            ("p/package-lock.json", false),
            ("p/dist", true),
            ("p/debug.log", false),
        ] {
            assert_eq!(f.decide(&r(p), dir, &sib, false), Decision::Include, "{p}");
        }
    }

    #[test]
    fn exclude_glob_semantics_follow_gitignore() {
        let f = filter(
            &[],
            &[],
            &[
                "/recorder/downloads/",
                "*.log",
                "**/gen/apple/Externals/",
                "a/b",
            ],
        );
        assert!(excluded(f.decide(
            &r("recorder/downloads"),
            true,
            &[],
            false
        )));
        assert_eq!(
            f.decide(&r("x/recorder/downloads"), true, &[], false),
            Decision::Include
        );
        assert_eq!(
            f.decide(&r("recorder/downloads"), false, &[], false),
            Decision::Include
        );
        assert!(excluded(f.decide(&r("deep/er/c.log"), false, &[], false)));
        assert!(excluded(f.decide(
            &r("webshop/src-tauri/gen/apple/Externals"),
            true,
            &[],
            false
        )));
        assert!(excluded(f.decide(&r("a/b"), false, &[], false)));
        assert_eq!(f.decide(&r("x/a/b"), false, &[], false), Decision::Include);
        assert_eq!(
            f.decide(&r("q.log"), false, &[], false),
            Decision::Exclude(Reason::Exclude("*.log".into()))
        );
    }

    #[test]
    fn include_beats_exclude_and_rules() {
        let f = filter(&[RulePack::Dev], &["target/keep.txt", "*.keep"], &["*.log"]);
        assert_eq!(
            f.decide(&r("target/keep.txt"), false, &[], true),
            Decision::Include
        );
        assert_eq!(
            f.decide(&r("a/x.keep"), false, &[], false),
            Decision::Include
        );
        assert_eq!(
            f.decide(&r("a/x.keep.log"), false, &[], false),
            Decision::Exclude(Reason::Exclude("*.log".into()))
        );
    }

    #[test]
    fn children_of_an_excluded_dir_inherit_the_exclusion() {
        let f = filter(&[], &["/big/keep/"], &["/big/"]);
        assert_eq!(
            f.decide(&r("big/other"), false, &[], true),
            Decision::Exclude(Reason::Inherited)
        );
        assert_eq!(f.decide(&r("big/keep"), true, &[], true), Decision::Include);
    }

    #[test]
    fn may_include_below_is_conservative() {
        let f = filter(&[], &["/big/keep/x.txt"], &[]);
        assert!(f.may_include_below(&r("big")));
        assert!(f.may_include_below(&r("big/keep")));
        assert!(!f.may_include_below(&r("other")));
        let any = filter(&[], &["x.txt"], &[]);
        assert!(any.may_include_below(&r("whatever/deep")));
        let wild = filter(&[], &["/a/*/keep"], &[]);
        assert!(wild.may_include_below(&r("a/b")));
        assert!(!filter(&[], &[], &[]).may_include_below(&r("a")));
    }

    #[test]
    fn secrets_match_builtins_and_extras() {
        let f = Filter::new(&[], &[], &[], &s(&["*.p12"])).unwrap();
        for p in [
            "a/.env",
            "a/.env.local",
            "k/server.pem",
            "id_ed25519",
            ".ssh/id_rsa.pub",
            "c/cert.p12",
        ] {
            assert!(f.is_secret(&r(p)), "{p}");
        }
        assert!(!f.is_secret(&r("a/env.rs")));
    }

    #[test]
    fn invalid_patterns_are_errors() {
        assert!(Filter::new(&[], &[], &s(&["a["]), &[]).is_err());
        assert!(Filter::new(&[], &s(&["/"]), &[], &[]).is_err());
        assert!(validate_pattern("  ").is_err());
        assert!(validate_pattern("ok/*.txt").is_ok());
    }

    #[test]
    fn labels_and_pack_selection() {
        let t = RULES.iter().find(|r| r.name == "target").unwrap();
        assert_eq!(t.label(), "target/ next to Cargo.toml");
        let ds = RULES.iter().find(|r| r.name == ".DS_Store").unwrap();
        assert_eq!(ds.label(), ".DS_Store");
        assert!(
            rules_for(&[RulePack::Junk])
                .iter()
                .all(|r| r.pack == RulePack::Junk)
        );
        assert_eq!(rules_for(&[RulePack::Dev]).len(), RULES.len());
        assert_eq!(Reason::Exclude("*.log".into()).label(), "exclude *.log");
    }
}
