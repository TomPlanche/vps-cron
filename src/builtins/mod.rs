//! Job implementations available to the scheduler.
//!
//! [`shell`] provides the generic command runner used by most jobs; the others
//! hold built-ins that need typed access to an API.

use std::path::Path;

use anyhow::Context;

pub mod github;
pub mod lastfm;
pub mod shell;

/// Serialises `value` as pretty JSON into `folder/filename`.
///
/// Missing parent directories are created, so a `filename` such as
/// `exports/today.json` works without setting the directory up by hand.
///
/// Returns the path written, for the run summary. Note that `filename` is
/// joined onto `folder`, which means an absolute path replaces `folder`
/// entirely and `..` escapes it. That is deliberate: writing into a directory
/// served by a web server is a legitimate thing to want.
pub(crate) fn write_json<T: serde::Serialize>(
    folder: &str,
    filename: &str,
    value: &T,
) -> anyhow::Result<String> {
    anyhow::ensure!(
        !filename.starts_with('~'),
        "filename '{filename}' starts with '~', which is expanded by shells and not by \
         vps-cron. Write the path out in full."
    );

    let path = Path::new(folder).join(filename);

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create '{}'", parent.display()))?;
        }
    }

    let json = serde_json::to_string_pretty(value)
        .with_context(|| format!("Failed to serialise {filename}"))?;

    std::fs::write(&path, json).with_context(|| format!("Failed to write '{}'", path.display()))?;

    Ok(path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> String {
        let dir = std::env::temp_dir().join(format!("vps-cron-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.display().to_string()
    }

    #[test]
    fn creates_missing_parent_directories() {
        let dir = temp_dir("nested");
        let path = write_json(&dir, "exports/deep/out.json", &serde_json::json!({"a": 1})).unwrap();

        assert!(Path::new(&path).exists());
        assert!(std::fs::read_to_string(&path).unwrap().contains("\"a\""));
    }

    #[test]
    fn a_relative_escape_leaves_the_folder() {
        let base = temp_dir("escape");
        let inner = format!("{base}/inner");
        std::fs::create_dir_all(&inner).unwrap();

        write_json(&inner, "../out.json", &serde_json::json!({"a": 1})).unwrap();

        assert!(Path::new(&base).join("out.json").exists());
    }

    #[test]
    fn a_tilde_path_is_rejected_with_an_explanation() {
        let dir = temp_dir("tilde");
        let err = write_json(&dir, "~/out.json", &serde_json::json!({})).unwrap_err();

        assert!(err.to_string().contains("expanded by shells"));
    }
}
