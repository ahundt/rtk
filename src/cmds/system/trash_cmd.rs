//! Built-in `rtk trash` - moves paths to the system trash via the `trash`
//! crate. Mirrors `rm`'s exit-code shape: silent on success, error on failure.

use anyhow::Result;
use std::path::Path;

/// Move `paths` to the system trash.
///
/// Returns `Ok(true)` only when all requested non-empty paths were moved to
/// trash. Never panics; reports failures to stderr in the same shape as `rm`.
pub fn execute(paths: &[String]) -> Result<bool> {
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
    match trash::delete_all(&refs) {
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

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("rtk_trash_test_{name}"));
        fs::write(&p, "x").unwrap();
        p
    }

    fn rm(p: &PathBuf) {
        let _ = fs::remove_file(p);
    }

    #[test]
    fn t_empty() {
        assert!(!execute(&[]).unwrap());
    }

    #[test]
    fn t_missing() {
        assert!(!execute(&["/nope_rtk_trash_test".into()]).unwrap());
    }

    #[test]
    fn t_single() {
        let p = tmp("s");
        assert!(execute(&[p.to_string_lossy().into()]).unwrap());
        rm(&p); // best-effort cleanup if trash failed
    }

    #[test]
    fn t_multi() {
        let (a, b) = (tmp("a"), tmp("b"));
        assert!(execute(&[a.to_string_lossy().into(), b.to_string_lossy().into()]).unwrap());
        rm(&a);
        rm(&b);
    }

    #[test]
    fn t_partial_missing_returns_false() {
        let p = tmp("partial");
        assert!(
            !execute(&[p.to_string_lossy().into(), "/nope_rtk_trash_test_partial".into()])
                .unwrap()
        );
        rm(&p);
    }

    #[test]
    fn t_expand_tilde_simple() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
        assert_eq!(expand_tilde("~/src"), format!("{home}/src"));
    }

    #[test]
    fn t_expand_tilde_no_tilde() {
        assert_eq!(expand_tilde("/absolute/path"), "/absolute/path");
    }

    #[test]
    fn t_expand_tilde_user_literal() {
        assert_eq!(expand_tilde("~other/path"), "~other/path");
    }
}
