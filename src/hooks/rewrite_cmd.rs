//! Translates a raw shell command into its RTK-optimized equivalent.

use super::permissions::{check_command, PermissionVerdict};
use crate::discover::lexer::{first_unattestable_construct, UnattestableConstruct};
use crate::discover::registry;
use std::io::Write;

/// Run the `rtk rewrite` command.
///
/// Prints the RTK-rewritten command to stdout and exits with a code that tells
/// the caller how to handle permissions:
///
/// | Exit | Stdout   | Meaning                                                      |
/// |------|----------|--------------------------------------------------------------|
/// | 0    | rewritten| Rewrite allowed — hook may auto-allow the rewritten command. |
/// | 1    | (none)   | No RTK equivalent — hook passes through unchanged.           |
/// | 2    | (none)   | Deny rule matched — hook defers to Claude Code native deny.  |
/// | 3    | rewritten| Ask rule matched — hook rewrites but lets Claude Code prompt.|
pub fn run(cmd: &str) -> anyhow::Result<()> {
    let (excluded, transparent_prefixes) = crate::core::config::Config::load()
        .map(|c| (c.hooks.exclude_commands, c.hooks.transparent_prefixes))
        .unwrap_or_default();

    match evaluate(cmd, &excluded, &transparent_prefixes) {
        RewriteOutcome::Allow(rewritten) => {
            print!("{}", rewritten);
            let _ = std::io::stdout().flush();
            Ok(())
        }
        RewriteOutcome::Ask(rewritten) => {
            print!("{}", rewritten);
            let _ = std::io::stdout().flush();
            std::process::exit(3);
        }
        RewriteOutcome::Deny => std::process::exit(2),
        RewriteOutcome::Passthrough => std::process::exit(1),
    }
}

#[derive(Debug, PartialEq)]
enum RewriteOutcome {
    Allow(String),
    Passthrough,
    Deny,
    Ask(String),
}

fn evaluate(cmd: &str, excluded: &[String], transparent_prefixes: &[String]) -> RewriteOutcome {
    let verdict = check_command(cmd);
    evaluate_with_verdict(cmd, excluded, transparent_prefixes, verdict)
}

fn evaluate_with_verdict(
    cmd: &str,
    excluded: &[String],
    transparent_prefixes: &[String],
    verdict: PermissionVerdict,
) -> RewriteOutcome {
    if verdict == PermissionVerdict::Deny {
        return RewriteOutcome::Deny;
    }

    let unattestable = first_unattestable_construct(cmd);
    if unattestable == Some(UnattestableConstruct::Substitution) {
        return RewriteOutcome::Passthrough;
    }

    match registry::rewrite_command_with_policy(cmd, excluded, transparent_prefixes) {
        Some(rewrite) => {
            if rewrite.requires_ask
                || unattestable == Some(UnattestableConstruct::FileTargetRedirect)
            {
                RewriteOutcome::Ask(rewrite.command)
            } else if verdict == PermissionVerdict::Allow {
                RewriteOutcome::Allow(rewrite.command)
            } else {
                RewriteOutcome::Ask(rewrite.command)
            }
        }
        None => RewriteOutcome::Passthrough,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rewrite_command_no_prefixes(cmd: &str) -> Option<String> {
        registry::rewrite_command(cmd, &[], &[])
    }

    #[test]
    fn test_run_supported_command_succeeds() {
        assert!(rewrite_command_no_prefixes("git status").is_some());
    }

    #[test]
    fn test_run_unsupported_returns_none() {
        assert!(rewrite_command_no_prefixes("htop").is_none());
    }

    #[test]
    fn test_run_already_rtk_returns_some() {
        assert_eq!(
            rewrite_command_no_prefixes("rtk git status"),
            Some("rtk git status".into())
        );
    }

    mod unattestable_passthrough {
        use super::super::{evaluate_with_verdict, RewriteOutcome};
        use crate::hooks::permissions::PermissionVerdict;

        #[test]
        fn test_unattestable_constructs_passthrough_under_default_verdict() {
            for (case, cmd) in [
                ("backtick substitution", "git status `rm -rf /tmp/x`"),
                ("dollar substitution", "git status $(rm -rf /tmp/x)"),
                (
                    "double-quoted substitution",
                    "git log --pretty=\"$(rm -rf /tmp/x)\"",
                ),
            ] {
                assert_eq!(
                    evaluate_with_verdict(cmd, &[], &[], PermissionVerdict::Default),
                    RewriteOutcome::Passthrough,
                    "{case} should pass through without consulting local permission files: {cmd}"
                );
            }
        }

        #[test]
        fn test_file_target_redirect_rewrites_as_ask_under_default_verdict() {
            for (cmd, rewritten) in [
                (
                    "git status > /tmp/rtk-status.txt",
                    "rtk git status > /tmp/rtk-status.txt",
                ),
                (
                    "cargo test >> /tmp/rtk-test.log",
                    "rtk cargo test >> /tmp/rtk-test.log",
                ),
                (
                    "cargo test >> /tmp/rtk-test.log 2>&1 | tail -50",
                    "rtk cargo test >> /tmp/rtk-test.log 2>&1 | tail -50",
                ),
            ] {
                assert_eq!(
                    evaluate_with_verdict(cmd, &[], &[], PermissionVerdict::Default),
                    RewriteOutcome::Ask(rewritten.to_string()),
                    "{cmd} should recover a rewrite but still require approval"
                );
            }
        }

        #[test]
        fn test_file_target_redirect_never_auto_allows() {
            assert_eq!(
                evaluate_with_verdict(
                    "git status > /tmp/rtk-status.txt",
                    &[],
                    &[],
                    PermissionVerdict::Allow,
                ),
                RewriteOutcome::Ask("rtk git status > /tmp/rtk-status.txt".into()),
                "file-target redirects must not become auto-allow rewrites"
            );
        }

        #[test]
        fn test_input_redirect_stays_passthrough() {
            assert_eq!(
                evaluate_with_verdict(
                    "cat < /tmp/rtk-input.txt",
                    &[],
                    &[],
                    PermissionVerdict::Default,
                ),
                RewriteOutcome::Passthrough,
                "input redirects are not a safe output suffix to reattach"
            );
        }

        #[test]
        fn test_deny_still_wins_for_file_target_redirect() {
            assert_eq!(
                evaluate_with_verdict(
                    "git status > /tmp/rtk-status.txt",
                    &[],
                    &[],
                    PermissionVerdict::Deny,
                ),
                RewriteOutcome::Deny
            );
        }

        #[test]
        fn test_default_verdict_still_rewrites_attestable_commands() {
            for (case, cmd) in [
                ("plain command", "git status"),
                ("fd dup redirect", "git status 2>&1"),
            ] {
                assert!(
                    matches!(
                        evaluate_with_verdict(cmd, &[], &[], PermissionVerdict::Default),
                        RewriteOutcome::Ask(_)
                    ),
                    "{case} should still rewrite with an ask/default verdict: {cmd}"
                );
            }
        }

        #[test]
        fn test_explicit_deny_still_wins_before_passthrough() {
            assert_eq!(
                evaluate_with_verdict(
                    "git status $(rm -rf /tmp/x)",
                    &[],
                    &[],
                    PermissionVerdict::Deny,
                ),
                RewriteOutcome::Deny,
            );
        }
    }

    /// SECURITY: Verify the exit code protocol for permission verdicts.
    ///
    /// The bash hook (.claude/hooks/rtk-rewrite.sh) interprets exit codes as:
    ///   0 → auto-allow (sets permissionDecision: "allow")
    ///   1 → passthrough (no RTK equivalent)
    ///   2 → deny (let Claude Code handle natively)
    ///   3 → ask (rewrite but omit permissionDecision, forcing user prompt)
    ///
    /// CRITICAL: PermissionVerdict::Default MUST map to exit 3 (ask), NOT exit 0.
    /// If Default were mapped to exit 0, any command without an explicit permission
    /// rule would be auto-allowed — bypassing Claude Code's least-privilege default.
    /// See: https://github.com/rtk-ai/rtk/issues/1155
    mod exit_code_protocol {
        use super::registry;
        use crate::hooks::permissions::{check_command_with_rules, PermissionVerdict};

        /// Exit code that `run()` returns for each verdict:
        ///   Allow  → 0 (exit Ok(()))
        ///   Ask    → 3 (process::exit(3))
        ///   Default→ 3 (process::exit(3)) — grouped with Ask
        ///   Deny   → 2 (process::exit(2)) — handled before rewrite match
        fn expected_exit_code(verdict: &PermissionVerdict) -> i32 {
            match verdict {
                PermissionVerdict::Allow => 0,
                PermissionVerdict::Deny => 2,
                PermissionVerdict::Ask => 3,
                PermissionVerdict::Default => 3, // MUST be 3, not 0!
            }
        }

        #[test]
        fn test_default_verdict_maps_to_ask_exit_code() {
            // When no rules match, verdict is Default → exit code must be 3 (ask).
            let verdict = check_command_with_rules("git status", &[], &[], &[]);
            assert_eq!(verdict, PermissionVerdict::Default);
            assert_eq!(
                expected_exit_code(&verdict),
                3,
                "Default verdict MUST exit with code 3 (ask), not 0 (allow)"
            );
        }

        #[test]
        fn test_allow_verdict_maps_to_allow_exit_code() {
            let allow = vec!["git *".to_string()];
            let verdict = check_command_with_rules("git status", &[], &[], &allow);
            assert_eq!(verdict, PermissionVerdict::Allow);
            assert_eq!(expected_exit_code(&verdict), 0);
        }

        #[test]
        fn test_ask_verdict_maps_to_ask_exit_code() {
            let ask = vec!["git push".to_string()];
            let verdict = check_command_with_rules("git push origin main", &[], &ask, &[]);
            assert_eq!(verdict, PermissionVerdict::Ask);
            assert_eq!(expected_exit_code(&verdict), 3);
        }

        #[test]
        fn test_deny_verdict_maps_to_deny_exit_code() {
            let deny = vec!["rm -rf".to_string()];
            let verdict = check_command_with_rules("rm -rf /tmp/test", &deny, &[], &[]);
            assert_eq!(verdict, PermissionVerdict::Deny);
            assert_eq!(expected_exit_code(&verdict), 2);
        }

        #[test]
        fn test_no_auto_allow_bypass_for_unrecognized_commands() {
            // SECURITY: A command with no permission rules and no matching allow rule
            // must NOT be auto-allowed. This is the core of issue #1155.
            // Even though `git status` can be rewritten to `rtk git status`,
            // the absence of an allow rule means Default → exit 3 → ask.
            let verdict = check_command_with_rules("git status", &[], &[], &[]);
            assert_eq!(verdict, PermissionVerdict::Default);

            // Verify the rewrite exists (so the hook would output it),
            // but the exit code forces user confirmation.
            assert!(registry::rewrite_command("git status", &[], &[]).is_some());
            assert_eq!(expected_exit_code(&verdict), 3);
        }

        #[test]
        fn test_default_never_equals_allow() {
            // Sentinel: ensure Default and Allow are distinct enum variants.
            // If this ever fails, the entire permission model is broken.
            assert_ne!(PermissionVerdict::Default, PermissionVerdict::Allow);
        }
    }
}
