//! Smart visibility and transfer selection. Exclusion never authorizes deletion.
use crate::{backend, config::Config};
use anyhow::{bail, Context, Result};
use globset::{GlobBuilder, GlobMatcher};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::OnceLock};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FilterConfig {
    pub temporary_files: bool,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub profiles: BTreeMap<String, ProfileFilter>,
}
impl Default for FilterConfig {
    fn default() -> Self {
        Self {
            temporary_files: true,
            include: vec![],
            exclude: vec![],
            profiles: BTreeMap::new(),
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProfileFilter {
    pub temporary_files: Option<bool>,
    pub include: Option<Vec<String>>,
    pub exclude: Option<Vec<String>>,
}
const TEMPORARY: &[&str] = &[
    ".DS_Store",
    "._*",
    "*.tmp",
    "*.swp",
    "*.swo",
    "*.part",
    "~$*",
    ".~lock.*",
];

fn compile(pattern: &str) -> Result<GlobMatcher> {
    if pattern.is_empty()
        || pattern.len() > 4096
        || pattern.starts_with('/')
        || pattern.contains('\\')
        || pattern.chars().any(char::is_control)
        || pattern.split('/').any(|part| part == ".." || part == ".")
    {
        bail!("invalid relative glob pattern");
    }
    let pattern = pattern.strip_suffix('/').unwrap_or(pattern);
    // An empty input and slash-only absolute path were rejected above.
    Ok(GlobBuilder::new(pattern)
        .literal_separator(true)
        .backslash_escape(false)
        .build()
        .context("invalid glob pattern")?
        .compile_matcher())
}
/// Slashless globs match the basename at any depth; slash globs are rooted.
/// A trailing slash describes a directory (and its descendants via allowed).
pub fn matches_pattern(pattern: &str, path: &str) -> Result<bool> {
    let path = backend::normalize(path)?;
    let matcher = compile(pattern)?;
    Ok(
        matcher.is_match(if pattern.trim_end_matches('/').contains('/') {
            path.as_str()
        } else {
            path.rsplit('/').next().unwrap_or("")
        }),
    )
}
impl FilterConfig {
    pub fn validate(&self) -> Result<()> {
        let mut groups = vec![self.include.as_slice(), self.exclude.as_slice()];
        for profile in self.profiles.values() {
            if let Some(items) = &profile.include {
                groups.push(items);
            }
            if let Some(items) = &profile.exclude {
                groups.push(items);
            }
        }
        for group in groups {
            if group.len() > 256 {
                bail!("at most 256 patterns per rule list");
            }
            for pattern in group {
                compile(pattern)?;
            }
        }
        Ok(())
    }
}
/// Precompiled once for an index enumeration/search; parent exclusions dominate.
pub struct Selector {
    temporary: bool,
    include: Vec<(String, GlobMatcher)>,
    exclude: Vec<(String, GlobMatcher)>,
}
impl Selector {
    pub fn new(config: &Config, profile: &str) -> Result<Self> {
        let rules = &config.filters;
        let local = rules.profiles.get(profile);
        let include = local
            .and_then(|p| p.include.as_ref())
            .unwrap_or(&rules.include);
        let exclude = local
            .and_then(|p| p.exclude.as_ref())
            .unwrap_or(&rules.exclude);
        let prepare = |patterns: &[String]| -> Result<Vec<(String, GlobMatcher)>> {
            patterns
                .iter()
                .map(|p| Ok((p.clone(), compile(p)?)))
                .collect()
        };
        Ok(Self {
            temporary: local
                .and_then(|p| p.temporary_files)
                .unwrap_or(rules.temporary_files),
            include: prepare(include)?,
            exclude: prepare(exclude)?,
        })
    }
    pub fn allowed(&self, path: &str, is_dir: bool) -> Result<bool> {
        Ok(self.exclusion_reason(path, is_dir)?.is_none())
    }
    pub fn exclusion_reason(&self, path: &str, is_dir: bool) -> Result<Option<String>> {
        let path = backend::normalize(path)?;
        if path.is_empty() {
            return Ok(None);
        }
        let matches = |rules: &[(String, GlobMatcher)], part: &str, directory: bool| {
            rules.iter().position(|(pattern, matcher)| {
                (!pattern.ends_with('/') || directory)
                    && matcher.is_match(if pattern.trim_end_matches('/').contains('/') {
                        part
                    } else {
                        part.rsplit('/').next().unwrap_or("")
                    })
            })
        };
        // Check ancestors too, including directories omitted by an earlier rule.
        for end in path
            .match_indices('/')
            .map(|(end, _)| end)
            .chain(std::iter::once(path.len()))
        {
            let part = &path[..end];
            let directory = end != path.len() || is_dir;
            let name = part.rsplit('/').next().unwrap_or("");
            if let Some(index) = matches(&self.exclude, part, directory) {
                let pattern = &self.exclude[index].0;
                return Ok(Some(format!("Excluded by pattern {pattern} at {part}.")));
            }
            static TEMP_MATCHERS: OnceLock<Vec<GlobMatcher>> = OnceLock::new();
            if let Some(index) = self.temporary.then(|| TEMP_MATCHERS
                    .get_or_init(|| {
                        TEMPORARY
                            .iter()
                            .map(|pattern| compile(pattern).expect("valid built-in temporary glob"))
                            .collect()
                    })
                    .iter()
                    .position(|matcher| matcher.is_match(name))).flatten()
            {
                return Ok(Some(format!("Excluded by built-in temporary-file rule {} at {part}.", TEMPORARY[index])));
            }
        }
        // Keep directory traversal possible when the allowlist contains files.
        Ok(if is_dir || self.include.is_empty() || matches(&self.include, &path, false).is_some() {
            None
        } else {
            Some("The file does not match the configured include patterns.".into())
        })
    }
}
pub fn allowed(config: &Config, profile: &str, path: &str, is_dir: bool) -> Result<bool> {
    Selector::new(config, profile)?.allowed(path, is_dir)
}
pub fn exclusion_reason(config: &Config, profile: &str, path: &str, is_dir: bool) -> Result<Option<String>> {
    Selector::new(config, profile)?.exclusion_reason(path, is_dir)
}

/// Recognize both the legacy mislabeled network failure and the typed local
/// exclusion without treating arbitrary server failures as repairable.
pub fn exclusion_path(message: &str) -> Option<&str> {
    ["excluded_by_filter: synchronization filters exclude ", "server_unreachable: synchronization filters exclude "]
        .iter().find_map(|prefix| message.strip_prefix(*prefix))?
        .strip_suffix("; local mutation retained until the rules allow it")
        .filter(|path| !path.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn glob_paths_and_inherited_directory_exclusions() {
        let mut config = Config::default();
        config.filters.include = vec!["*.txt".into(), "docs/**/*.md".into()];
        config.filters.exclude = vec!["private/".into(), "docs/draft/**".into()];
        let select = Selector::new(&config, "p").unwrap();
        assert!(select.allowed("中文/报告.txt", false).unwrap());
        assert!(select.allowed("docs/deep/readme.md", false).unwrap());
        assert!(select.allowed("docs", true).unwrap());
        assert!(!select.allowed("private/sub/a.txt", false).unwrap());
        assert!(!select.allowed("docs/draft/secret.md", false).unwrap());
        assert!(!select.allowed("deep/.DS_Store", false).unwrap());
        assert!(!select.allowed("doc.pdf", false).unwrap());
        assert!(!matches_pattern("docs/*.md", "docs/deep/a.md").unwrap());
        assert!(matches_pattern("*.txt", "deep/a.txt").unwrap());
    }
    #[test]
    fn overrides_and_invalid_patterns() {
        let mut config = Config::default();
        config.filters.profiles.insert(
            "p".into(),
            ProfileFilter {
                temporary_files: Some(false),
                include: Some(vec![]),
                exclude: Some(vec!["秘密/**".into()]),
            },
        );
        assert!(allowed(&config, "p", "a.tmp", false).unwrap());
        assert!(!allowed(&config, "q", "a.tmp", false).unwrap());
        for pattern in ["/root", "/", "../secret", "[", "a\\b", ""] {
            assert!(compile(pattern).is_err());
        }
    }

    #[test]
    fn empty_root_is_traversable_and_oversized_rule_lists_are_rejected() {
        let mut config = Config::default();
        assert!(Selector::new(&config, "p").unwrap().allowed("", true).unwrap());
        config.filters.exclude = vec!["*.tmp".into(); 257];
        assert!(config.filters.validate().unwrap_err().to_string().contains("256"));
    }

    #[test]
    fn reasons_identify_inherited_rules_and_share_the_visibility_decision() {
        let mut config = Config::default();
        assert!(exclusion_reason(&config,"p","Backup/book.part",false).unwrap().unwrap().contains("temporary-file rule *.part"));
        assert!(exclusion_reason(&config,"p","Backup/.DS_Store/child",false).unwrap().unwrap().contains(".DS_Store"));
        assert_eq!(exclusion_reason(&config,"p","",true).unwrap(),None);
        assert_eq!(exclusion_reason(&config,"p","Backup/book.epub",false).unwrap(),None);
        config.filters.exclude = vec!["private/".into()];
        assert!(exclusion_reason(&config,"p","private/child/book.epub",false).unwrap().unwrap().contains("pattern private/ at private"));
        config.filters.include = vec!["*.epub".into()];
        assert!(exclusion_reason(&config,"p","book.pdf",false).unwrap().unwrap().contains("include patterns"));
        assert_eq!(exclusion_reason(&config,"p","Backup",true).unwrap(),None);
        config.filters.profiles.insert("p".into(), ProfileFilter { temporary_files:Some(false),include:Some(vec![]),exclude:Some(vec![])});
        config.filters.validate().unwrap();
        assert_eq!(exclusion_reason(&config,"p","Backup/book.part",false).unwrap(),None);
        assert!(exclusion_reason(&config,"p","../escape",false).is_err());
        config.filters.exclude = vec!["[".into()];
        assert!(exclusion_reason(&config,"other","book.epub",false).is_err());
        for prefix in ["server_unreachable", "excluded_by_filter"] {
            let message = format!("{prefix}: synchronization filters exclude Backup/book.part; local mutation retained until the rules allow it");
            assert_eq!(exclusion_path(&message),Some("Backup/book.part"));
        }
        for message in ["server_unreachable: timed out", "excluded_by_filter: synchronization filters exclude Backup/book.part", "excluded_by_filter: synchronization filters exclude ; local mutation retained until the rules allow it"] {
            assert_eq!(exclusion_path(message),None);
        }
    }
}
