//! Path derivation shared by CLI commands that resolve modules
//! relative to an input script.

use std::path::{Path, PathBuf};

/// Return the filesystem root that owns an entry script's sibling modules.
///
/// Bare filenames have an empty lexical parent, so normalize that case to
/// `.`. Nested relative paths stay relative to the caller's current working
/// directory, while absolute entry paths naturally produce an absolute root.
pub fn module_root(path: &Path) -> PathBuf {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_root_handles_bare_nested_and_absolute_entries() {
        assert_eq!(module_root(Path::new("main.nybl")), PathBuf::from("."));
        assert_eq!(
            module_root(Path::new("scripts/nested/main.nybl")),
            PathBuf::from("scripts/nested")
        );

        let absolute = std::env::temp_dir()
            .join("nybl-entry-path")
            .join("main.nybl");
        assert_eq!(
            module_root(&absolute),
            absolute.parent().unwrap().to_path_buf()
        );
    }
}
