//! Built-in `rtk trash` - moves paths to the system trash via the `trash`
//! crate. Mirrors `rm`'s exit-code shape: silent on success, error on failure.

use anyhow::Result;
use std::fmt::Display;
use std::path::{Path, PathBuf};

/// Move `paths` to the system trash.
///
/// Returns `Ok(true)` only when all requested non-empty paths were moved to
/// trash. Never panics; reports failures to stderr in the same shape as `rm`.
pub fn execute(paths: &[PathBuf]) -> Result<bool> {
    execute_with_delete(paths, |paths| trash::delete_all(paths))
}

fn execute_with_delete<F, E>(paths: &[PathBuf], delete_all: F) -> Result<bool>
where
    F: FnOnce(&[PathBuf]) -> std::result::Result<(), E>,
    E: Display,
{
    let expanded: Vec<PathBuf> = paths
        .iter()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| expand_tilde(p))
        .collect();

    if expanded.is_empty() {
        eprintln!("trash: no paths specified");
        return Ok(false);
    }

    let (existing, missing): (Vec<_>, Vec<_>) = expanded.into_iter().partition(|p| path_exists(p));

    for p in &missing {
        eprintln!("trash: cannot remove '{}': No such path", p.display());
    }

    if existing.is_empty() {
        return Ok(false);
    }

    match delete_all(&existing) {
        Ok(_) => Ok(missing.is_empty()),
        Err(e) => {
            eprintln!("trash: {}", e);
            Ok(false)
        }
    }
}

/// CLI entry point for `rtk trash <paths...>`. Returns the exit code (0 on
/// success, 1 if any path failed).
pub fn run(paths: &[PathBuf]) -> Result<i32> {
    let ok = execute(paths)?;
    Ok(if ok { 0 } else { 1 })
}

#[cfg(test)]
fn run_with_delete<F, E>(paths: &[PathBuf], delete_all: F) -> Result<i32>
where
    F: FnOnce(&[PathBuf]) -> std::result::Result<(), E>,
    E: Display,
{
    let ok = execute_with_delete(paths, delete_all)?;
    Ok(if ok { 0 } else { 1 })
}

/// Expand `~` or `~/...` to the current user's home directory.
fn expand_tilde(path: &Path) -> PathBuf {
    let Some(path_str) = path.to_str() else {
        return path.to_path_buf();
    };
    let Some(home) = dirs::home_dir() else {
        return path.to_path_buf();
    };
    if path_str == "~" {
        return home;
    }
    if let Some(rest) = path_str
        .strip_prefix("~/")
        .or_else(|| path_str.strip_prefix("~\\"))
    {
        return home.join(rest);
    }
    path.to_path_buf()
}

fn path_exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
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

    fn fake_delete_all(paths: &[PathBuf]) -> std::io::Result<()> {
        for path in paths {
            fs::remove_file(path)?;
        }
        Ok(())
    }

    #[test]
    fn execute_rejects_empty_or_missing_inputs() {
        let cases = [
            ("no paths", vec![]),
            ("empty paths only", vec![PathBuf::new()]),
            ("missing path", vec![PathBuf::from("/nope_rtk_trash_test")]),
        ];

        for (name, paths) in cases {
            assert!(!execute(&paths).unwrap(), "{name}");
        }
    }

    #[test]
    fn execute_trashes_existing_paths() {
        let (_dir, paths) = fixture_files(&["one.txt", "two.txt"]);
        assert!(execute_with_delete(&paths, fake_delete_all).unwrap());
        assert!(paths.iter().all(|path| !path.exists()), "{paths:?}");
    }

    #[test]
    fn execute_reports_partial_missing_paths_as_failure() {
        let (_dir, paths) = fixture_files(&["partial.txt"]);
        assert!(
            !execute_with_delete(
                &[
                    paths[0].clone(),
                    PathBuf::from("/nope_rtk_trash_test_partial")
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
        assert_eq!(run_with_delete(&[paths[0].clone()], fake_delete_all).unwrap(), 0);
    }

    #[test]
    fn expand_tilde_handles_only_current_user_home() {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        let cases = [
            (PathBuf::from("~"), home.clone()),
            (PathBuf::from("~/src"), home.join("src")),
            (PathBuf::from("~\\src"), home.join("src")),
            (PathBuf::from("/absolute/path"), PathBuf::from("/absolute/path")),
            (PathBuf::from("~other/path"), PathBuf::from("~other/path")),
        ];

        for (input, expected) in cases {
            assert_eq!(expand_tilde(&input), expected, "{}", input.display());
        }
    }
}
