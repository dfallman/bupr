//! `RelPath`: a validated, '/'-separated path relative to a preset root. It can
//! never contain `..`, `.`, empty components, a leading `/` or NUL, so joining
//! it onto a root cannot climb out of that root (spec §3.4).

use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid relative path {path:?}: {why}")]
pub struct RelPathError {
    pub path: String,
    pub why: &'static str,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RelPath(String);

impl RelPath {
    pub fn new(s: &str) -> Result<Self, RelPathError> {
        let fail = |why| {
            Err(RelPathError {
                path: s.to_string(),
                why,
            })
        };
        if s.is_empty() {
            return fail("empty");
        }
        if s.starts_with('/') {
            return fail("absolute");
        }
        if s.contains('\0') {
            return fail("contains NUL");
        }
        for c in s.split('/') {
            match c {
                "" => return fail("empty component"),
                "." | ".." => return fail("dot component"),
                _ => {}
            }
        }
        Ok(Self(s.to_string()))
    }

    pub fn from_path(p: &Path) -> Result<Self, RelPathError> {
        match p.to_str() {
            Some(s) => Self::new(s),
            None => Err(RelPathError {
                path: p.to_string_lossy().into_owned(),
                why: "not UTF-8",
            }),
        }
    }

    /// `parent/name`, or `name` when `parent` is the root (`None`).
    pub fn child(parent: Option<&RelPath>, name: &str) -> Result<Self, RelPathError> {
        if name.contains('/') {
            return Err(RelPathError {
                path: name.to_string(),
                why: "name contains '/'",
            });
        }
        match parent {
            Some(p) => Self::new(&format!("{}/{}", p.0, name)),
            None => Self::new(name),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn as_path(&self) -> &Path {
        Path::new(&self.0)
    }

    pub fn parent(&self) -> Option<RelPath> {
        self.0.rsplit_once('/').map(|(p, _)| RelPath(p.to_string()))
    }

    pub fn file_name(&self) -> &str {
        self.0.rsplit('/').next().unwrap_or_default()
    }

    pub fn depth(&self) -> usize {
        self.0.matches('/').count() + 1
    }

    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }

    /// True if `self` equals `other` or lies beneath it.
    pub fn starts_with(&self, other: &RelPath) -> bool {
        self.0 == other.0
            || (self.0.len() > other.0.len()
                && self.0.starts_with(&other.0)
                && self.0.as_bytes()[other.0.len()] == b'/')
    }
}

impl TryFrom<String> for RelPath {
    type Error = RelPathError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        RelPath::new(&s)
    }
}

impl From<RelPath> for String {
    fn from(r: RelPath) -> String {
        r.0
    }
}

impl fmt::Display for RelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_normal_paths() {
        for s in ["a", "a/b", ".git", "a/.env", "...", "-rf", " lead", "å/ä"] {
            assert_eq!(RelPath::new(s).unwrap().as_str(), s);
        }
    }

    #[test]
    fn rejects_escaping_or_malformed_paths() {
        for s in [
            "", "/a", "a//b", "a/", "./a", "a/./b", "..", "a/../b", "../a", "a\0b",
        ] {
            assert!(RelPath::new(s).is_err(), "{s:?} must be rejected");
        }
    }

    #[test]
    fn navigation() {
        let p = RelPath::new("a/b/c.txt").unwrap();
        assert_eq!(p.file_name(), "c.txt");
        assert_eq!(p.parent().unwrap().as_str(), "a/b");
        assert_eq!(p.depth(), 3);
        assert_eq!(p.components().collect::<Vec<_>>(), ["a", "b", "c.txt"]);
        assert!(RelPath::new("a").unwrap().parent().is_none());
    }

    #[test]
    fn child_joins_and_validates() {
        let a = RelPath::new("a").unwrap();
        assert_eq!(RelPath::child(Some(&a), "b").unwrap().as_str(), "a/b");
        assert_eq!(RelPath::child(None, "b").unwrap().as_str(), "b");
        assert!(RelPath::child(Some(&a), "..").is_err());
        assert!(RelPath::child(Some(&a), "x/y").is_err());
    }

    #[test]
    fn starts_with_is_component_wise() {
        let ab = RelPath::new("a/b").unwrap();
        assert!(ab.starts_with(&RelPath::new("a").unwrap()));
        assert!(ab.starts_with(&ab));
        assert!(
            !RelPath::new("ab/c")
                .unwrap()
                .starts_with(&RelPath::new("a").unwrap())
        );
    }

    #[test]
    fn serde_round_trip_and_validation() {
        let p = RelPath::new("a/b").unwrap();
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(json, "\"a/b\"");
        assert_eq!(serde_json::from_str::<RelPath>(&json).unwrap(), p);
        assert!(serde_json::from_str::<RelPath>("\"../x\"").is_err());
    }
}
