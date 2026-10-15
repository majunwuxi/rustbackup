use std::path::{Component, Path};

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use thiserror::Error;

use crate::FilterConfig;

#[derive(Debug, Error)]
pub enum FilterError {
    #[error("exclude pattern cannot be empty")]
    EmptyPattern,
    #[error("invalid exclude pattern: {0}")]
    InvalidPattern(#[from] globset::Error),
}

pub struct FilterSet {
    patterns: GlobSet,
    case_insensitive: bool,
}

impl FilterSet {
    pub fn new(config: &FilterConfig) -> Result<Self, FilterError> {
        let mut builder = GlobSetBuilder::new();
        for pattern in &config.exclude {
            if pattern.is_empty() {
                return Err(FilterError::EmptyPattern);
            }
            if is_directory_exclusion(pattern, config.case_insensitive) {
                continue;
            }
            builder.add(
                GlobBuilder::new(pattern)
                    .case_insensitive(config.case_insensitive)
                    .build()?,
            );
        }
        Ok(Self {
            patterns: builder.build()?,
            case_insensitive: config.case_insensitive,
        })
    }

    /// Returns true for unsafe rooted/parent paths as well as excluded paths.
    /// Directory exclusions match exact path components and can prune a walk.
    pub fn is_excluded(&self, relative_path: &Path, is_directory: bool) -> bool {
        let mut components = relative_path.components().peekable();
        while let Some(component) = components.next() {
            let Component::Normal(name) = component else {
                return true;
            };
            let name = name.to_string_lossy();
            let is_last = components.peek().is_none();

            if is_directory_name(&name, self.case_insensitive) && (!is_last || is_directory) {
                return true;
            }
            if self.patterns.is_match(name.as_ref()) {
                return true;
            }
        }
        false
    }
}

fn is_directory_exclusion(pattern: &str, case_insensitive: bool) -> bool {
    is_directory_name(pattern, case_insensitive)
}

fn is_directory_name(name: &str, case_insensitive: bool) -> bool {
    if case_insensitive {
        name.eq_ignore_ascii_case(".git") || name.eq_ignore_ascii_case("node_modules")
    } else {
        name == ".git" || name == "node_modules"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_filters() -> FilterSet {
        FilterSet::new(&FilterConfig {
            exclude: vec![
                "*.tmp".to_owned(),
                "~$*".to_owned(),
                ".git".to_owned(),
                "node_modules".to_owned(),
            ],
            case_insensitive: true,
        })
        .expect("default filter rules should compile")
    }

    #[test]
    fn filename_patterns_match_at_every_depth_and_ignore_case() {
        let filters = default_filters();
        assert!(filters.is_excluded(Path::new("docs/old.TMP"), false));
        assert!(filters.is_excluded(Path::new("docs/~$Budget.xlsx"), false));
        assert!(!filters.is_excluded(Path::new("docs/Budget.xlsx"), false));
    }

    #[test]
    fn excluded_directories_match_exact_components_and_can_be_pruned() {
        let filters = default_filters();
        assert!(filters.is_excluded(Path::new("project/.GiT/config"), false));
        assert!(filters.is_excluded(Path::new("project/Node_Modules/pkg"), false));
        assert!(filters.is_excluded(Path::new("project/node_modules"), true));
        assert!(!filters.is_excluded(Path::new("project/my-node_modules"), true));
        assert!(!filters.is_excluded(Path::new("project/.git"), false));
    }

    #[test]
    fn rooted_and_parent_relative_paths_fail_closed() {
        let filters = default_filters();
        assert!(filters.is_excluded(Path::new("../outside.txt"), false));
        assert!(filters.is_excluded(Path::new("/outside.txt"), false));
    }
}
