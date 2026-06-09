//! Built-in `rtk trash` — moves paths to the system trash via the `trash`
//! crate. Mirrors `rm`'s exit-code semantics: silent on success, error on
//! failure. Powers the `rm-to-trash` safety rewrite when
//! `RTK_SAFE_COMMANDS=1` is set.

use anyhow::Result;
use std::path::Path;

/// Move `paths` to the system trash.
///
/// Returns `Ok(true)` if at least one path was trashed, `Ok(false)` if the
/// list was empty or every path was missing. Never panics; reports failures
/// to stderr in the same shape as `rm`.
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
        Ok(_) => Ok(true),
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

/// Expand a leading `~` to `$HOME` (or `$USERPROFILE` on Windows).
fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix('~') {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap_or_else(|_| "/".to_string());
        format!("{home}{rest}")
    } else {
        path.to_string()
    }
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
    fn t_expand_tilde_simple() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
        assert_eq!(expand_tilde("~/src"), format!("{home}/src"));
    }

    #[test]
    fn t_expand_tilde_no_tilde() {
        assert_eq!(expand_tilde("/absolute/path"), "/absolute/path");
    }
}
