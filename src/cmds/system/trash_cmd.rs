//! Built-in `rtk trash` - moves paths to the system trash via the `trash`
//! crate. Mirrors `rm`'s exit-code shape: silent on success, error on failure.

use anyhow::Result;
use std::{fmt::Display, path::Path};

/// Move `paths` to the system trash.
///
/// Returns `Ok(true)` only when all requested non-empty paths were moved to
/// trash. Never panics; reports failures to stderr in the same shape as `rm`.
pub fn execute(paths: &[String]) -> Result<bool> {
    execute_with_delete(paths, |paths| trash::delete_all(paths))
}

fn execute_with_delete<F, E>(paths: &[String], delete_all: F) -> Result<bool>
where
    F: FnOnce(&[&str]) -> std::result::Result<(), E>,
    E: Display,
{
    let expanded: Vec<String> = paths
        .iter()
        .filter(|p| !p.is_empty())
        .map(|p| expand_tilde(p))
        .collect();

    if expanded.is_empty() {
        eprintln!("trash: no paths specified");
        return Ok(false);
    }

    let (existing, missing): (Vec<_>, Vec<_>) =
        expanded.iter().partition(|p| Path::new(p).exists());

    for p in &missing {
        eprintln!("trash: cannot remove '{}': No such path", p);
    }

    if existing.is_empty() {
        return Ok(false);
    }

    let refs: Vec<&str> = existing.iter().map(|s| s.as_str()).collect();
    match delete_all(&refs) {
        Ok(_) => Ok(missing.is_empty()),
        Err(e) => {
            eprintln!("trash: {}", e);
            Ok(false)
        }
    }
}

/// CLI entry point for `rtk trash <paths...>`. Returns the exit code (0 on
/// success, 1 if any path failed).
pub fn run(paths: &[String]) -> Result<i32> {
    let ok = execute(paths)?;
    Ok(if ok { 0 } else { 1 })
}

#[cfg(test)]
fn run_with_delete<F, E>(paths: &[String], delete_all: F) -> Result<i32>
where
    F: FnOnce(&[&str]) -> std::result::Result<(), E>,
    E: Display,
{
    let ok = execute_with_delete(paths, delete_all)?;
    Ok(if ok { 0 } else { 1 })
}

/// Expand `~` or `~/...` to the current user's home directory.
fn expand_tilde(path: &str) -> String {
    if path == "~" || path.starts_with("~/") {
        if let Some(home) = dirs::home_dir() {
            if path == "~" {
                return home.to_string_lossy().into_owned();
            }
            return home
                .join(path.strip_prefix("~/").unwrap_or_default())
                .to_string_lossy()
                .into_owned();
        }
    }
    path.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn fixture_files(names: &[&str]) -> (TempDir, Vec<PathBuf>) {
        let dir = tempfile::tempdir().expect("create temp dir");
        let paths = names
            .iter()
            .map(|name| {
                let path = dir.path().join(name);
                fs::write(&path, "x").expect("write trash test fixture");
                path
            })
            .collect();
        (dir, paths)
    }

    fn fake_delete_all(paths: &[&str]) -> std::io::Result<()> {
        for path in paths {
            fs::remove_file(path)?;
        }
        Ok(())
    }

    #[test]
    fn execute_rejects_empty_or_missing_inputs() {
        let cases = [
            ("no paths", vec![]),
            ("empty strings only", vec!["".to_string()]),
            ("missing path", vec!["/nope_rtk_trash_test".to_string()]),
        ];

        for (name, paths) in cases {
            assert!(!execute(&paths).unwrap(), "{name}");
        }
    }

    #[test]
    fn execute_trashes_existing_paths() {
        let (_dir, paths) = fixture_files(&["one.txt", "two.txt"]);
        let args: Vec<String> = paths
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect();

        assert!(execute_with_delete(&args, fake_delete_all).unwrap());
        assert!(paths.iter().all(|path| !path.exists()), "{paths:?}");
    }

    #[test]
    fn execute_reports_partial_missing_paths_as_failure() {
        let (_dir, paths) = fixture_files(&["partial.txt"]);
        assert!(
            !execute_with_delete(
                &[
                    paths[0].to_string_lossy().into_owned(),
                    "/nope_rtk_trash_test_partial".to_string()
                ],
                fake_delete_all
            )
            .unwrap(),
            "partial success must keep rm-style non-zero semantics"
        );
    }

    #[test]
    fn run_maps_boolean_result_to_exit_code() {
        let (_dir, paths) = fixture_files(&["exit-code.txt"]);
        assert_eq!(run_with_delete(&[], fake_delete_all).unwrap(), 1);
        assert_eq!(
            run_with_delete(&[paths[0].to_string_lossy().into_owned()], fake_delete_all).unwrap(),
            0
        );
    }

    #[test]
    fn expand_tilde_handles_only_current_user_home() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
        let cases = [
            ("~", home.clone()),
            ("~/src", format!("{home}/src")),
            ("/absolute/path", "/absolute/path".to_string()),
            ("~other/path", "~other/path".to_string()),
        ];

        for (input, expected) in cases {
            assert_eq!(expand_tilde(input), expected, "{input}");
        }
    }
}
