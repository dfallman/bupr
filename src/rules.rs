//! Built-in rule packs and include/exclude matching (spec §5).
//!
//! Precedence per entry: include glob → included; inside an excluded
//! directory → excluded (inherited); exclude glob → excluded; rule-pack rule
//! (name + sibling marker) → excluded; otherwise included. `.gitignore` is
//! never consulted here.

use std::cell::RefCell;

use globset::{Glob, GlobBuilder, GlobMatcher};
use serde::{Deserialize, Serialize};

use crate::relpath::RelPath;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RulePack {
    Dev,
    Junk,
    Video,
    Music,
}

impl RulePack {
    pub const ALL: [RulePack; 4] = [
        RulePack::Dev,
        RulePack::Junk,
        RulePack::Video,
        RulePack::Music,
    ];

    pub fn name(self) -> &'static str {
        match self {
            RulePack::Dev => "dev",
            RulePack::Junk => "junk",
            RulePack::Video => "video",
            RulePack::Music => "music",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            RulePack::Dev => "build output and dependencies of common toolchains (includes junk)",
            RulePack::Junk => "OS clutter: .DS_Store, AppleDouble files, editor swap files",
            RulePack::Video => {
                "render, analysis, and audio caches of Final Cut Pro and Premiere Pro"
            }
            RulePack::Music => "fade and waveform caches of Pro Tools, Cubase, and Reaper",
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
const FCP_EVENT: &[&str] = &["CurrentVersion.fcpevent"];
const PREMIERE: &[&str] = &["*.prproj"];
const PRO_TOOLS: &[&str] = &["*.ptx", "*.ptf"];

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
    // Only caches the app rebuilds from the project itself. Proxies,
    // optimized media and freeze files are kept: rebuilding them needs the
    // original media or the same plugins, which may be gone.
    rule(RulePack::Video, "Render Files", Applies::Dir, FCP_EVENT),
    rule(RulePack::Video, "Analysis Files", Applies::Dir, FCP_EVENT),
    rule(
        RulePack::Video,
        "Adobe Premiere Pro Video Previews",
        Applies::Dir,
        PREMIERE,
    ),
    rule(
        RulePack::Video,
        "Adobe Premiere Pro Audio Previews",
        Applies::Dir,
        PREMIERE,
    ),
    rule(RulePack::Video, "*.pek", Applies::File, &[]),
    rule(RulePack::Video, "*.cfa", Applies::File, &[]),
    rule(RulePack::Music, "Fade Files", Applies::Dir, PRO_TOOLS),
    rule(RulePack::Music, "WaveCache.wfm", Applies::File, PRO_TOOLS),
    rule(RulePack::Music, "Images", Applies::Dir, &["*.cpr"]),
    rule(RulePack::Music, "*.reapeaks", Applies::File, &[]),
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
    let active =
        |p: RulePack| packs.contains(&p) || (p == RulePack::Junk && packs.contains(&RulePack::Dev));
    RULES.iter().filter(|r| active(r.pack)).collect()
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
    dir_only: bool,
    matcher: GlobMatcher,
    /// The normalized glob (anchored, or prefixed with `**/`) split at `/`.
    parts: Vec<Part>,
}

/// One `/`-separated component of a normalized glob.
enum Part {
    /// `**`: any number of components.
    Any,
    One(GlobMatcher),
}

impl Pattern {
    fn matches(&self, rel: &RelPath, is_dir: bool) -> bool {
        (!self.dir_only || is_dir) && self.matcher.is_match(rel.as_str())
    }

    /// Could this pattern match something strictly below `dir`? A leading
    /// `**` only reaches into `dir` when a trailing run of its components
    /// matches the start of the rest of the pattern.
    fn may_match_below(&self, dir: &[&str]) -> bool {
        match self.parts.split_first() {
            Some((Part::Any, rest)) => (0..dir.len()).any(|s| open_prefix(rest, &dir[s..])),
            _ => open_prefix(&self.parts, dir),
        }
    }
}

/// Does `dir` match the first `dir.len()` parts, with at least one part left
/// for what lies below it? Reaching a `**` matches anything from there on.
fn open_prefix(parts: &[Part], dir: &[&str]) -> bool {
    for (i, d) in dir.iter().enumerate() {
        match parts.get(i) {
            None => return false,
            Some(Part::Any) => return true,
            Some(Part::One(m)) if !m.is_match(d) => return false,
            Some(Part::One(_)) => {}
        }
    }
    dir.len() < parts.len()
}

fn glob_matcher(glob: &str, p: &str) -> Result<GlobMatcher, String> {
    Ok(GlobBuilder::new(glob)
        .literal_separator(true)
        .build()
        .map_err(|e| format!("invalid pattern {p:?}: {e}"))?
        .compile_matcher())
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
    let parts = glob
        .split('/')
        .map(|c| match c {
            "**" => Ok(Part::Any),
            _ => glob_matcher(c, p).map(Part::One),
        })
        .collect::<Result<_, _>>()?;
    Ok(Pattern {
        original: p.to_string(),
        dir_only,
        matcher: glob_matcher(&glob, p)?,
        parts,
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

/// A directory listing handed to [`Filter::decide`] for each of its entries.
/// Whether any sibling matches a rule's markers is computed at most once per
/// rule, and only when an entry's name first needs it. Use one per directory
/// and one filter.
pub struct Siblings<'a> {
    names: &'a [String],
    markers: RefCell<Vec<Option<bool>>>,
}

impl<'a> Siblings<'a> {
    pub fn new(names: &'a [String]) -> Siblings<'a> {
        Siblings {
            names,
            markers: RefCell::default(),
        }
    }

    fn has_marker(&self, rule: usize, markers: &[GlobMatcher]) -> bool {
        let mut cache = self.markers.borrow_mut();
        if cache.len() <= rule {
            cache.resize(rule + 1, None);
        }
        *cache[rule].get_or_insert_with(|| {
            self.names
                .iter()
                .any(|s| markers.iter().any(|m| m.is_match(s)))
        })
    }
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
        siblings: &Siblings,
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
        for (i, r) in self.rules.iter().enumerate() {
            let kind_ok = match r.applies {
                Applies::File => !is_dir,
                Applies::Dir => is_dir,
                Applies::Any => true,
            };
            if kind_ok
                && r.name.is_match(name)
                && (r.markers.is_empty() || siblings.has_marker(i, &r.markers))
            {
                return Decision::Exclude(Reason::Rule(r.label.clone()));
            }
        }
        Decision::Include
    }

    /// Must the excluded directory `dir` still be walked because an include
    /// may match below it (spec §5.1)? As in gitignore, an excluded folder
    /// is only entered for a pattern that names it: an anchored pattern whose
    /// leading components match `dir`, or a `**/` pattern whose next
    /// components match `dir`'s last ones. A bare `*.keep` never enters one.
    pub fn may_include_below(&self, dir: &RelPath) -> bool {
        let dir: Vec<&str> = dir.components().collect();
        self.include.iter().any(|p| p.may_match_below(&dir))
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
    fn dec(f: &Filter, p: &str, is_dir: bool, sib: &[&str], parent: bool) -> Decision {
        f.decide(&r(p), is_dir, &Siblings::new(&s(sib)), parent)
    }
    fn excluded(d: Decision) -> bool {
        matches!(d, Decision::Exclude(_))
    }

    #[test]
    fn target_excluded_only_next_to_cargo_toml() {
        let f = filter(&[RulePack::Dev], &[], &[]);
        let d = dec(
            &f,
            "tools/target",
            true,
            &["Cargo.toml", "src", "target"],
            false,
        );
        assert_eq!(
            d,
            Decision::Exclude(Reason::Rule("target/ next to Cargo.toml".into()))
        );
        assert_eq!(
            dec(&f, "notes/target", true, &["target", "todo.md"], false),
            Decision::Include
        );
        // A *file* called target is never a build dir.
        assert_eq!(
            dec(&f, "x/target", false, &["Cargo.toml", "target"], false),
            Decision::Include
        );
    }

    #[test]
    fn build_dir_needs_a_project_marker() {
        let f = filter(&[RulePack::Dev], &[], &[]);
        assert!(excluded(dec(
            &f,
            "x/build",
            true,
            &["app.xcodeproj", "build"],
            false
        )));
        assert!(excluded(dec(
            &f,
            "x/build",
            true,
            &["build.gradle.kts", "build"],
            false
        )));
        assert_eq!(
            dec(&f, "x/build", true, &["build", "README.md"], false),
            Decision::Include
        );
    }

    #[test]
    fn marker_globs_match() {
        let f = filter(&[RulePack::Dev], &[], &[]);
        assert!(excluded(dec(
            &f,
            "w/.next",
            true,
            &["next.config.mjs"],
            false
        )));
        assert!(excluded(dec(
            &f,
            "w/.svelte-kit",
            true,
            &["svelte.config.js"],
            false
        )));
    }

    #[test]
    fn junk_is_part_of_dev_and_default() {
        for packs in [&[RulePack::Dev][..], &[RulePack::Junk][..]] {
            let f = filter(packs, &[], &[]);
            assert!(excluded(dec(&f, "a/.DS_Store", false, &[], false)));
            assert!(excluded(dec(&f, "a/._foo", false, &[], false)));
            assert!(excluded(dec(&f, "a/x.swp", false, &[], false)));
        }
        let junk_only = filter(&[RulePack::Junk], &[], &[]);
        assert_eq!(
            dec(&junk_only, "p/target", true, &["Cargo.toml"], false),
            Decision::Include
        );
    }

    #[test]
    fn video_caches_need_their_project() {
        let f = filter(&[RulePack::Video], &[], &[]);
        let event = &["CurrentVersion.fcpevent", "Original Media", "Render Files"];
        for d in ["Render Files", "Analysis Files"] {
            assert!(
                excluded(dec(&f, &format!("L.fcpbundle/E/{d}"), true, event, false)),
                "{d}"
            );
            assert_eq!(
                dec(&f, &format!("x/{d}"), true, &[d], false),
                Decision::Include
            );
        }
        for d in ["Original Media", "Transcoded Media", "Shared Items"] {
            assert_eq!(
                dec(&f, &format!("L.fcpbundle/E/{d}"), true, event, false),
                Decision::Include
            );
        }
        let premiere = &["cut.prproj", "Adobe Premiere Pro Auto-Save"];
        for d in [
            "Adobe Premiere Pro Video Previews",
            "Adobe Premiere Pro Audio Previews",
        ] {
            assert!(
                excluded(dec(&f, &format!("p/{d}"), true, premiere, false)),
                "{d}"
            );
            assert_eq!(
                dec(&f, &format!("p/{d}"), true, &[], false),
                Decision::Include
            );
        }
        assert_eq!(
            dec(&f, "p/Adobe Premiere Pro Auto-Save", true, premiere, false),
            Decision::Include
        );
        assert!(excluded(dec(&f, "m/clip.mov.pek", false, &[], false)));
        assert!(excluded(dec(&f, "m/clip.mov 48000.cfa", false, &[], false)));
        assert_eq!(dec(&f, "a/.DS_Store", false, &[], false), Decision::Include);
    }

    #[test]
    fn music_caches_need_their_session() {
        let f = filter(&[RulePack::Music], &[], &[]);
        let pt = &["song.ptx", "Audio Files", "Session File Backups"];
        assert!(excluded(dec(&f, "s/Fade Files", true, pt, false)));
        assert!(excluded(dec(&f, "s/WaveCache.wfm", false, pt, false)));
        assert!(excluded(dec(&f, "s/Fade Files", true, &["old.ptf"], false)));
        for d in ["Audio Files", "Session File Backups", "Bounced Files"] {
            assert_eq!(
                dec(&f, &format!("s/{d}"), true, pt, false),
                Decision::Include
            );
        }
        assert_eq!(dec(&f, "x/Fade Files", true, &[], false), Decision::Include);
        assert!(excluded(dec(
            &f,
            "c/Images",
            true,
            &["song.cpr", "Audio"],
            false
        )));
        assert_eq!(
            dec(&f, "c/Images", true, &["index.html"], false),
            Decision::Include
        );
        assert_eq!(
            dec(&f, "c/Edits", true, &["song.cpr"], false),
            Decision::Include
        );
        assert!(excluded(dec(&f, "r/vox.wav.reapeaks", false, &[], false)));
        // Ableton .asd files hold saved warp markers, so they are kept.
        assert_eq!(
            dec(&f, "a/kick.wav.asd", false, &[], false),
            Decision::Include
        );
    }

    #[test]
    fn no_packs_includes_everything() {
        let f = filter(&[], &[], &[]);
        assert_eq!(dec(&f, "a/.DS_Store", false, &[], false), Decision::Include);
    }

    #[test]
    fn private_but_important_files_are_never_excluded_by_rules() {
        let f = filter(&[RulePack::Dev], &[], &[]);
        let sib = &["Cargo.toml", "package.json", "composer.json"];
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
            assert_eq!(dec(&f, p, dir, sib, false), Decision::Include, "{p}");
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
        assert!(excluded(dec(&f, "recorder/downloads", true, &[], false)));
        assert_eq!(
            dec(&f, "x/recorder/downloads", true, &[], false),
            Decision::Include
        );
        assert_eq!(
            dec(&f, "recorder/downloads", false, &[], false),
            Decision::Include
        );
        assert!(excluded(dec(&f, "deep/er/c.log", false, &[], false)));
        assert!(excluded(dec(
            &f,
            "webshop/src-tauri/gen/apple/Externals",
            true,
            &[],
            false
        )));
        assert!(excluded(dec(&f, "a/b", false, &[], false)));
        assert_eq!(dec(&f, "x/a/b", false, &[], false), Decision::Include);
        assert_eq!(
            dec(&f, "q.log", false, &[], false),
            Decision::Exclude(Reason::Exclude("*.log".into()))
        );
    }

    #[test]
    fn include_beats_exclude_and_rules() {
        let f = filter(&[RulePack::Dev], &["target/keep.txt", "*.keep"], &["*.log"]);
        assert_eq!(
            dec(&f, "target/keep.txt", false, &[], true),
            Decision::Include
        );
        assert_eq!(dec(&f, "a/x.keep", false, &[], false), Decision::Include);
        assert_eq!(
            dec(&f, "a/x.keep.log", false, &[], false),
            Decision::Exclude(Reason::Exclude("*.log".into()))
        );
    }

    #[test]
    fn children_of_an_excluded_dir_inherit_the_exclusion() {
        let f = filter(&[], &["/big/keep/"], &["/big/"]);
        assert_eq!(
            dec(&f, "big/other", false, &[], true),
            Decision::Exclude(Reason::Inherited)
        );
        assert_eq!(dec(&f, "big/keep", true, &[], true), Decision::Include);
    }

    #[test]
    fn anchored_includes_walk_only_their_own_path() {
        let f = filter(&[], &["/big/keep/x.txt"], &[]);
        assert!(f.may_include_below(&r("big")));
        assert!(f.may_include_below(&r("big/keep")));
        assert!(!f.may_include_below(&r("big/keep/x.txt")));
        assert!(!f.may_include_below(&r("other")));
        assert!(!f.may_include_below(&r("x/big")));
        let wild = filter(&[], &["/a/*/keep"], &[]);
        assert!(wild.may_include_below(&r("a/b")));
        assert!(!wild.may_include_below(&r("b/a")));
        let deep = filter(&[], &["/a/**/keep"], &[]);
        assert!(deep.may_include_below(&r("a/b/c/d")));
        assert!(!deep.may_include_below(&r("b/c")));
        assert!(!filter(&[], &[], &[]).may_include_below(&r("a")));
    }

    #[test]
    fn unanchored_includes_walk_only_folders_they_name() {
        for p in ["x.txt", "*.keep"] {
            let any = filter(&[], &[p], &[]);
            assert!(!any.may_include_below(&r("whatever/deep")), "{p}");
            assert!(!any.may_include_below(&r("node_modules")), "{p}");
        }
        let nm = filter(&[], &["**/node_modules/keep.txt"], &[]);
        assert!(nm.may_include_below(&r("x/node_modules")));
        assert!(nm.may_include_below(&r("node_modules")));
        assert!(!nm.may_include_below(&r("target")));
        assert!(!nm.may_include_below(&r("x/node_modules/y")));
        let wild = filter(&[], &["**/a/*/keep"], &[]);
        assert!(wild.may_include_below(&r("p/a")));
        assert!(wild.may_include_below(&r("p/a/b")));
        assert!(!wild.may_include_below(&r("p/b")));
        assert!(!wild.may_include_below(&r("p/a/b/c")));
        let deep = filter(&[], &["**/a/**/keep"], &[]);
        assert!(deep.may_include_below(&r("p/a/b/c")));
        assert!(!deep.may_include_below(&r("p/b/c")));
    }

    #[test]
    fn marker_presence_is_cached_per_rule() {
        let f = filter(&[RulePack::Dev], &[], &[]);
        let names = s(&["Cargo.toml", "package.json", "node_modules", "target"]);
        let sib = Siblings::new(&names);
        for n in ["target", "node_modules", "target"] {
            assert!(excluded(f.decide(&r(n), true, &sib, false)), "{n}");
        }
        assert_eq!(f.decide(&r("build"), true, &sib, false), Decision::Include);
        let cached = sib.markers.borrow().iter().filter(|m| m.is_some()).count();
        assert_eq!(cached, 3, "target, node_modules and build markers");
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
        let dev = rules_for(&[RulePack::Dev]);
        assert!(dev.iter().any(|r| r.pack == RulePack::Junk));
        assert!(
            dev.iter()
                .all(|r| matches!(r.pack, RulePack::Dev | RulePack::Junk))
        );
        for pack in [RulePack::Video, RulePack::Music] {
            let rules = rules_for(&[pack]);
            assert!(!rules.is_empty());
            assert!(
                rules.iter().all(|r| r.pack == pack),
                "{pack:?} implies nothing"
            );
        }
        assert_eq!(Reason::Exclude("*.log".into()).label(), "exclude *.log");
    }
}
