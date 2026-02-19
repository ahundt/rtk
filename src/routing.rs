//! Static routing table: maps external binary commands to RTK subcommands.
//!
//! This is the single source of truth for which commands route through RTK and how.
//! Used by both `cmd::hook` (runtime routing) and `discover::registry` (pattern generation).
//!
//! # Adding a new command
//!
//! 1. Add one `Route` entry to `ROUTES` below.
//! 2. Add discover metadata (RtkRule) to `src/discover/registry.rs` if needed.
//! 3. Done — routing is automatic.
//!
//! Complex cases (vitest bare invocation, `uv pip`, `python -m pytest`, pnpm, npx)
//! require Rust logic and stay as match arms in `cmd::hook::route_native_command`.

use std::collections::HashMap;
use std::sync::OnceLock;

/// Subcommand filter for a route entry.
#[derive(Debug, Clone, Copy)]
pub enum Subcmds {
    /// Route ALL subcommands of this binary (e.g., ls, curl, prettier).
    Any,
    /// Route ONLY these specific subcommands; others fall through to `rtk run -c`.
    Only(&'static [&'static str]),
}

/// One row in the static routing table.
///
/// - `binaries`: one or more external binary names mapping to the same RTK subcommand.
/// - `subcmds`: subcommand filter — `Any` matches everything, `Only` restricts to a list.
/// - `rtk_cmd`: the RTK subcommand name (e.g., `"grep"`, `"lint"`, `"git"`).
///
/// For direct routes where `binary == rtk_cmd`, the hook uses `format!("rtk {raw}")`.
/// For renames (`rg` → `grep`, `eslint` → `lint`), it uses `replace_first_word`.
#[derive(Debug, Clone, Copy)]
pub struct Route {
    pub binaries: &'static [&'static str],
    pub subcmds: Subcmds,
    pub rtk_cmd: &'static str,
}

/// Static routing table. Single source of truth for hook routing.
///
/// Order does not matter — lookups use a HashMap built once at startup (O(1) per call).
///
/// Complex cases not representable as simple table entries stay in Rust match arms:
/// - `vitest` (bare invocation → `rtk vitest run`)
/// - `uv pip` (prefix replacement)
/// - `python`/`python3 -m pytest` (two-level prefix replacement)
/// - `pnpm` (complex sub-routing via `route_pnpm`)
/// - `npx` (complex sub-routing via `route_npx`)
pub const ROUTES: &[Route] = &[
    // Version control
    Route {
        binaries: &["git"],
        subcmds: Subcmds::Only(&[
            "status", "diff", "log", "add", "commit", "push", "pull", "branch", "fetch", "stash",
            "show",
        ]),
        rtk_cmd: "git",
    },
    // GitHub CLI
    Route {
        binaries: &["gh"],
        subcmds: Subcmds::Only(&["pr", "issue", "run"]),
        rtk_cmd: "gh",
    },
    // Rust build tools
    Route {
        binaries: &["cargo"],
        subcmds: Subcmds::Only(&["test", "build", "clippy", "check"]),
        rtk_cmd: "cargo",
    },
    // Search — two binaries, one RTK subcommand (rename)
    Route {
        binaries: &["rg", "grep"],
        subcmds: Subcmds::Any,
        rtk_cmd: "grep",
    },
    // JavaScript linting — rename
    Route {
        binaries: &["eslint"],
        subcmds: Subcmds::Any,
        rtk_cmd: "lint",
    },
    // File system
    Route {
        binaries: &["ls"],
        subcmds: Subcmds::Any,
        rtk_cmd: "ls",
    },
    // TypeScript compiler
    Route {
        binaries: &["tsc"],
        subcmds: Subcmds::Any,
        rtk_cmd: "tsc",
    },
    // JavaScript formatting
    Route {
        binaries: &["prettier"],
        subcmds: Subcmds::Any,
        rtk_cmd: "prettier",
    },
    // E2E testing
    Route {
        binaries: &["playwright"],
        subcmds: Subcmds::Any,
        rtk_cmd: "playwright",
    },
    // Database ORM
    Route {
        binaries: &["prisma"],
        subcmds: Subcmds::Any,
        rtk_cmd: "prisma",
    },
    // Network
    Route {
        binaries: &["curl"],
        subcmds: Subcmds::Any,
        rtk_cmd: "curl",
    },
    // Python testing
    Route {
        binaries: &["pytest"],
        subcmds: Subcmds::Any,
        rtk_cmd: "pytest",
    },
    // Log tailing
    Route {
        binaries: &["tail"],
        subcmds: Subcmds::Any,
        rtk_cmd: "tail",
    },
    // Go linting
    Route {
        binaries: &["golangci-lint"],
        subcmds: Subcmds::Any,
        rtk_cmd: "golangci-lint",
    },
    // Containers — read-only subcommands only
    Route {
        binaries: &["docker"],
        subcmds: Subcmds::Only(&["ps", "images", "logs"]),
        rtk_cmd: "docker",
    },
    // Kubernetes — read-only subcommands only
    Route {
        binaries: &["kubectl"],
        subcmds: Subcmds::Only(&["get", "logs"]),
        rtk_cmd: "kubectl",
    },
    // Go build tools
    Route {
        binaries: &["go"],
        subcmds: Subcmds::Only(&["test", "build", "vet"]),
        rtk_cmd: "go",
    },
    // Python linting/formatting
    Route {
        binaries: &["ruff"],
        subcmds: Subcmds::Only(&["check", "format"]),
        rtk_cmd: "ruff",
    },
    // Python package management
    Route {
        binaries: &["pip"],
        subcmds: Subcmds::Only(&["list", "outdated", "install", "show"]),
        rtk_cmd: "pip",
    },
];

/// Look up the routing entry for a binary + subcommand.
///
/// Returns `Some(route)` if the binary is in the table AND the subcommand matches
/// the entry's filter. Returns `None` if unrecognised or subcommand not in `Only` list.
///
/// The HashMap is built once per process (OnceLock). Each binary maps to the index of
/// its `Route` in `ROUTES`. Multiple binaries from the same entry (e.g., `rg`/`grep`)
/// both point to the same index.
pub fn lookup(binary: &str, sub: &str) -> Option<&'static Route> {
    static MAP: OnceLock<HashMap<&'static str, usize>> = OnceLock::new();
    let map = MAP.get_or_init(|| {
        let mut m = HashMap::new();
        for (i, route) in ROUTES.iter().enumerate() {
            for &bin in route.binaries {
                m.entry(bin).or_insert(i);
            }
        }
        m
    });

    let idx = *map.get(binary)?;
    let route = &ROUTES[idx];

    let matches = match route.subcmds {
        Subcmds::Any => true,
        Subcmds::Only(subs) => subs.contains(&sub),
    };

    if matches {
        Some(route)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lookup_direct_route() {
        let r = lookup("git", "status").unwrap();
        assert_eq!(r.rtk_cmd, "git");
    }

    #[test]
    fn test_lookup_git_unknown_subcommand_returns_none() {
        assert!(lookup("git", "rebase").is_none());
        assert!(lookup("git", "bisect").is_none());
    }

    #[test]
    fn test_lookup_rename_rg_to_grep() {
        let r = lookup("rg", "").unwrap();
        assert_eq!(r.rtk_cmd, "grep");
    }

    #[test]
    fn test_lookup_rename_grep_to_grep() {
        let r = lookup("grep", "-r").unwrap();
        assert_eq!(r.rtk_cmd, "grep");
    }

    #[test]
    fn test_lookup_rename_eslint_to_lint() {
        let r = lookup("eslint", "src/").unwrap();
        assert_eq!(r.rtk_cmd, "lint");
    }

    #[test]
    fn test_lookup_any_subcommand() {
        // ls routes any subcommand
        let r = lookup("ls", "-la").unwrap();
        assert_eq!(r.rtk_cmd, "ls");
        let r2 = lookup("ls", "").unwrap();
        assert_eq!(r2.rtk_cmd, "ls");
    }

    #[test]
    fn test_lookup_unknown_binary_returns_none() {
        assert!(lookup("unknownbinary99", "").is_none());
        // These stay as complex Rust match arms, not in ROUTES
        assert!(lookup("vitest", "").is_none());
        assert!(lookup("pnpm", "list").is_none());
        assert!(lookup("npx", "tsc").is_none());
        assert!(lookup("uv", "pip").is_none());
    }

    #[test]
    fn test_lookup_docker_subcommand_filter() {
        assert!(lookup("docker", "ps").is_some());
        assert!(lookup("docker", "images").is_some());
        assert!(lookup("docker", "build").is_none());
        assert!(lookup("docker", "run").is_none());
    }

    #[test]
    fn test_lookup_cargo_subcommand_filter() {
        assert!(lookup("cargo", "test").is_some());
        assert!(lookup("cargo", "clippy").is_some());
        assert!(lookup("cargo", "publish").is_none());
    }

    #[test]
    fn test_no_duplicate_binaries_in_routes() {
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for route in ROUTES {
            for &bin in route.binaries {
                assert!(
                    seen.insert(bin),
                    "Binary '{bin}' appears in multiple ROUTES entries"
                );
            }
        }
    }

    #[test]
    fn test_lookup_is_o1_consistent() {
        // Calling lookup multiple times returns same result (OnceLock stable)
        let r1 = lookup("git", "status");
        let r2 = lookup("git", "status");
        assert_eq!(r1.map(|r| r.rtk_cmd), r2.map(|r| r.rtk_cmd));
    }
}
