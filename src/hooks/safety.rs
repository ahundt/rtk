//! Safety Policy Engine — portable, rule-based command safety.
//!
//! Ports v2's `cmd/safety.rs` + a minimal slice of `config/rules.rs`. Rules
//! are Markdown files with YAML frontmatter, compiled into the binary via
//! `include_str!()`. Opt-in via the `RTK_SAFE_COMMANDS=1` and
//! `RTK_BLOCK_TOKEN_WASTE=1` environment variables.
//!
//! Integration: hook handlers call `check(cmd)` BEFORE permission checks.
//! Default behaviour with no env vars set: passthrough (no changes).

use anyhow::{anyhow, Result};
use std::collections::{BTreeMap, HashMap};
use std::sync::OnceLock;

// =============================================================================
// Rule definition (deserialised from MD frontmatter)
// =============================================================================

/// A unified safety rule loaded from Markdown frontmatter.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Rule {
    pub name: String,
    #[serde(default)]
    pub patterns: Vec<String>,
    #[serde(default = "default_block")]
    pub action: String,
    #[serde(default)]
    pub redirect: Option<String>,
    #[serde(default = "default_always")]
    pub when: String,
    #[serde(default)]
    pub env_var: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(skip)]
    pub message: String,
    #[serde(skip)]
    pub source: String,
}

fn default_block() -> String {
    "block".into()
}
fn default_always() -> String {
    "always".into()
}
fn default_true() -> bool {
    true
}

impl Rule {
    /// True when the rule should fire given the current env + predicate state.
    pub fn should_apply(&self) -> bool {
        // The required env_var must be set to a truthy value (opt-IN). If unset
        // or "0"/"false", the rule is dormant. Default behaviour is OFF to
        // avoid surprising users who haven't opted in.
        if let Some(ref env) = self.env_var {
            match std::env::var(env) {
                Ok(val) if val != "0" && val != "false" => {}
                _ => return false,
            }
        }
        check_when(&self.when)
    }
}

// =============================================================================
// Result type returned by safety checks
// =============================================================================

/// Outcome of a safety check.
#[derive(Clone, Debug, PartialEq)]
pub enum SafetyResult {
    /// Command is safe to execute as-is.
    Safe,
    /// Command is blocked; the embedded string is the user-facing reason.
    Blocked(String),
    /// Command was rewritten into a safer form.
    Rewritten(String),
    /// Built-in trash request — paths to move to the system trash.
    TrashRequested(Vec<String>),
}

// =============================================================================
// Predicate registry — small set of context-aware predicates
// =============================================================================

type PredicateFn = fn() -> bool;

fn predicate_registry() -> &'static HashMap<&'static str, PredicateFn> {
    static REGISTRY: OnceLock<HashMap<&'static str, PredicateFn>> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let mut m = HashMap::new();
        m.insert("always", (|| true) as PredicateFn);
        m.insert("has_unstaged_changes", has_unstaged_changes as PredicateFn);
        m
    })
}

pub fn check_when(when: &str) -> bool {
    if when == "always" || when.is_empty() {
        return true;
    }
    if let Some(func) = predicate_registry().get(when) {
        return func();
    }
    // Last-resort fallback: treat the predicate as a shell expression. Mirrors
    // v2 behaviour so user-defined rules (when discovery is ported later) can
    // use bash predicates.
    std::process::Command::new("sh")
        .args(["-c", when])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// True iff `git diff --quiet` reports unstaged changes in the current repo.
fn has_unstaged_changes() -> bool {
    std::process::Command::new("git")
        .args(["diff", "--quiet"])
        .status()
        .map(|s| !s.success())
        .unwrap_or(false)
}

/// True iff stderr is a TTY (used to pick human vs agent block messages).
fn is_interactive() -> bool {
    use std::io::IsTerminal;
    std::io::stderr().is_terminal()
}

// =============================================================================
// Parsing — MD frontmatter → Rule
// =============================================================================

pub fn parse_rule(content: &str, source: &str) -> Result<Rule> {
    let trimmed = content.trim();
    let rest = trimmed
        .strip_prefix("---")
        .ok_or_else(|| anyhow!("No frontmatter: missing opening ---"))?;
    let end = rest
        .find("\n---")
        .ok_or_else(|| anyhow!("Unclosed frontmatter: missing closing ---"))?;
    let yaml = &rest[..end];
    let body = rest[end + 4..].trim();
    let mut rule: Rule = serde_yaml::from_str(yaml)?;
    rule.message = body.to_string();
    rule.source = source.to_string();
    Ok(rule)
}

// =============================================================================
// Built-in default rules (compiled into the binary)
// =============================================================================

pub const DEFAULT_RULES: &[&str] = &[
    include_str!("../rules/rtk.safety.rm-to-trash.md"),
    include_str!("../rules/rtk.safety.git-reset-hard.md"),
    include_str!("../rules/rtk.safety.git-checkout-dashdash.md"),
    include_str!("../rules/rtk.safety.git-checkout-dot.md"),
    include_str!("../rules/rtk.safety.git-stash-drop.md"),
    include_str!("../rules/rtk.safety.git-clean-fd.md"),
    include_str!("../rules/rtk.safety.git-clean-df.md"),
    include_str!("../rules/rtk.safety.git-clean-f.md"),
    include_str!("../rules/rtk.safety.block-cat.md"),
    include_str!("../rules/rtk.safety.block-sed.md"),
    include_str!("../rules/rtk.safety.block-head.md"),
];

static RULES_CACHE: OnceLock<Vec<Rule>> = OnceLock::new();

/// Return the deduplicated set of compiled-in rules. Cached after first call.
///
/// This v3 port intentionally drops walk-up discovery (`~/.config/rtk/` and
/// `.rtk/`). Re-add it together with `config/rules.rs` + `config/discovery.rs`
/// in a follow-up PR (per plan §4b F2).
pub fn load_all() -> &'static [Rule] {
    RULES_CACHE.get_or_init(|| {
        let mut rules_by_name: BTreeMap<String, Rule> = BTreeMap::new();
        for content in DEFAULT_RULES {
            match parse_rule(content, "builtin") {
                Ok(rule) if rule.enabled => {
                    rules_by_name.insert(rule.name.clone(), rule);
                }
                Ok(rule) => {
                    rules_by_name.remove(&rule.name);
                }
                Err(e) => eprintln!("rtk: bad builtin safety rule: {e}"),
            }
        }
        rules_by_name.into_values().collect()
    })
}

// =============================================================================
// Global-option stripping — match patterns despite `git --no-pager reset` etc.
// =============================================================================

fn strip_global_options(full_cmd: &str) -> String {
    let words: Vec<&str> = full_cmd.split_whitespace().collect();
    if words.is_empty() {
        return full_cmd.to_string();
    }

    let binary = words[0];
    let rest = &words[1..];

    match binary {
        "git" => {
            let mut result = vec!["git"];
            let mut i = 0;
            while i < rest.len() {
                let w = rest[i];
                let is_kv_global = w.starts_with("--")
                    && w.contains('=')
                    && !w.starts_with("--hard")
                    && !w.starts_with("--force");
                let is_bool_global = matches!(
                    w,
                    "--no-pager"
                        | "--no-optional-locks"
                        | "--bare"
                        | "--literal-pathspecs"
                        | "--paginate"
                        | "--git-dir"
                );
                if (w == "-C" || w == "-c") && i + 1 < rest.len() {
                    i += 2;
                } else if is_kv_global || is_bool_global {
                    i += 1;
                } else {
                    result.extend_from_slice(&rest[i..]);
                    break;
                }
            }
            result.join(" ")
        }
        "cargo" => {
            let mut result = vec!["cargo"];
            let mut i = 0;
            while i < rest.len() {
                let w = rest[i];
                if w.starts_with('+') {
                    i += 1;
                } else {
                    result.extend_from_slice(&rest[i..]);
                    break;
                }
            }
            result.join(" ")
        }
        "docker" => {
            let mut result = vec!["docker"];
            let mut i = 0;
            while i < rest.len() {
                let w = rest[i];
                if matches!(w, "-H" | "--context" | "--config") && i + 1 < rest.len() {
                    i += 2;
                } else if w.starts_with("--") && w.contains('=') {
                    i += 1;
                } else {
                    result.extend_from_slice(&rest[i..]);
                    break;
                }
            }
            result.join(" ")
        }
        "kubectl" => {
            let mut result = vec!["kubectl"];
            let mut i = 0;
            while i < rest.len() {
                let w = rest[i];
                if matches!(w, "--context" | "--kubeconfig" | "--namespace" | "-n")
                    && i + 1 < rest.len()
                {
                    i += 2;
                } else if w.starts_with("--") && w.contains('=') {
                    i += 1;
                } else {
                    result.extend_from_slice(&rest[i..]);
                    break;
                }
            }
            result.join(" ")
        }
        _ => full_cmd.to_string(),
    }
}

// =============================================================================
// Pattern matching
// =============================================================================

/// Does a rule's pattern match `full_cmd`?
///
/// - Single-word pattern matches when `binary` equals the pattern (parsed
///   mode), or any whitespace-delimited word in `full_cmd` equals the pattern
///   (raw mode, `binary = None`).
/// - Multi-word patterns prefix-match `full_cmd` (with global options stripped).
pub fn matches_rule(rule: &Rule, binary: Option<&str>, full_cmd: &str) -> bool {
    rule.patterns.iter().any(|pat| {
        if pat.contains(' ') {
            let normalized = strip_global_options(full_cmd);
            full_cmd.starts_with(pat.as_str()) || normalized.starts_with(pat.as_str())
        } else if let Some(bin) = binary {
            bin == pat
        } else {
            full_cmd
                .split_whitespace()
                .any(|w| w == pat || w.ends_with(&format!("/{pat}")))
        }
    })
}

// =============================================================================
// Dispatch — matched rule → SafetyResult
// =============================================================================

fn dispatch(rule: &Rule, args: &str) -> SafetyResult {
    match rule.action.as_str() {
        "trash" => {
            let paths: Vec<String> = args
                .split_whitespace()
                .filter(|a| !a.starts_with('-'))
                .map(String::from)
                .collect();
            SafetyResult::TrashRequested(paths)
        }
        "rewrite" => {
            let redirect = rule.redirect.as_deref().unwrap_or(args);
            SafetyResult::Rewritten(redirect.replace("{args}", args))
        }
        "suggest_tool" | "block" => {
            let msg = if is_interactive() {
                if rule.action == "suggest_tool" {
                    rule.message
                        .lines()
                        .next()
                        .unwrap_or(&rule.message)
                        .to_string()
                } else {
                    rule.message.clone()
                }
            } else {
                rule.message.clone()
            };
            SafetyResult::Blocked(msg)
        }
        "warn" => {
            eprintln!("{}", rule.message);
            SafetyResult::Safe
        }
        _ => SafetyResult::Safe,
    }
}

// =============================================================================
// Public entry points
// =============================================================================

/// Check a parsed command (binary + args) against every loaded safety rule.
pub fn check(binary: &str, args: &[String]) -> SafetyResult {
    let full_cmd = if args.is_empty() {
        binary.to_string()
    } else {
        format!("{} {}", binary, args.join(" "))
    };

    for rule in load_all() {
        if !matches_rule(rule, Some(binary), &full_cmd) {
            continue;
        }
        if !rule.should_apply() {
            continue;
        }
        return dispatch(rule, &args.join(" "));
    }
    SafetyResult::Safe
}

/// Check a raw command string (used by hook payloads where the command has
/// not been parsed). Catches dangerous patterns even when we cannot split
/// flags from positional args reliably.
pub fn check_raw(raw: &str) -> SafetyResult {
    for rule in load_all() {
        if !matches_rule(rule, None, raw) {
            continue;
        }
        if !rule.should_apply() {
            continue;
        }
        // suggest_tool rules don't apply in raw mode — `cat` inside a pipeline
        // (`cat file | jq`) is legitimate and we can't tell from the string.
        if rule.action == "suggest_tool" {
            continue;
        }
        // Trash collapses to Block in raw mode — we cannot reliably split paths
        // from flags without parsing.
        if rule.action == "trash" {
            return SafetyResult::Blocked(format!(
                "Passthrough blocked: '{}' detected. Use native mode for safe trash.",
                rule.patterns.first().map(|s| s.as_str()).unwrap_or("rm")
            ));
        }
        return dispatch(rule, raw);
    }
    SafetyResult::Safe
}

// =============================================================================
// Tests — ported from v2 cmd/safety.rs + integration tests for permissions.rs
// =============================================================================

/// Shared mutex serialising env-var-mutating tests across the `safety` and
/// `permissions` modules. Both modules touch `RTK_SAFE_COMMANDS` /
/// `RTK_BLOCK_TOKEN_WASTE`; without a single shared lock, parallel tests
/// race each other through these globals.
#[cfg(test)]
pub(crate) fn test_env_lock() -> &'static std::sync::Mutex<()> {
    static ENV_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    ENV_LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::MutexGuard;

    // ---- env guard ---------------------------------------------------------
    // Serialise env-var-mutating tests; auto-cleanup on drop.

    struct EnvGuard {
        _lock: MutexGuard<'static, ()>,
    }

    impl EnvGuard {
        fn new() -> Self {
            let lock = test_env_lock().lock().unwrap_or_else(|e| e.into_inner());
            Self::cleanup();
            Self { _lock: lock }
        }

        fn cleanup() {
            std::env::remove_var("RTK_SAFE_COMMANDS");
            std::env::remove_var("RTK_BLOCK_TOKEN_WASTE");
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            Self::cleanup();
        }
    }

    // === Basic check tests ===

    #[test]
    fn test_check_safe_command() {
        let _guard = EnvGuard::new();
        assert_eq!(check("ls", &["-la".to_string()]), SafetyResult::Safe);
    }

    #[test]
    fn test_check_git_status() {
        let _guard = EnvGuard::new();
        assert_eq!(check("git", &["status".to_string()]), SafetyResult::Safe);
    }

    #[test]
    fn test_check_empty_args() {
        let _guard = EnvGuard::new();
        assert_eq!(check("pwd", &[]), SafetyResult::Safe);
    }

    // === rm safety (RTK_SAFE_COMMANDS) ===

    #[test]
    fn test_check_rm_redirected_when_env_set() {
        let _guard = EnvGuard::new();
        std::env::set_var("RTK_SAFE_COMMANDS", "1");
        match check("rm", &["file.txt".to_string()]) {
            SafetyResult::TrashRequested(paths) => assert_eq!(paths, vec!["file.txt"]),
            other => panic!("Expected TrashRequested, got {:?}", other),
        }
    }

    #[test]
    fn test_check_rm_safe_by_default() {
        let _guard = EnvGuard::new();
        // OPT-IN: with no env var set, rm should pass through unchanged.
        assert_eq!(
            check("rm", &["file.txt".to_string()]),
            SafetyResult::Safe,
            "rm must be passthrough when RTK_SAFE_COMMANDS is unset (opt-in default)"
        );
    }

    #[test]
    fn test_check_rm_safe_when_explicitly_disabled() {
        let _guard = EnvGuard::new();
        std::env::set_var("RTK_SAFE_COMMANDS", "0");
        assert_eq!(check("rm", &["file.txt".to_string()]), SafetyResult::Safe);
    }

    #[test]
    fn test_check_rm_with_flags() {
        let _guard = EnvGuard::new();
        std::env::set_var("RTK_SAFE_COMMANDS", "1");
        match check("rm", &["-rf".to_string(), "dir".to_string()]) {
            SafetyResult::TrashRequested(paths) => assert_eq!(paths, vec!["dir"]),
            other => panic!("Expected TrashRequested, got {:?}", other),
        }
    }

    #[test]
    fn test_check_rm_multiple_files() {
        let _guard = EnvGuard::new();
        std::env::set_var("RTK_SAFE_COMMANDS", "1");
        match check(
            "rm",
            &[
                "a.txt".to_string(),
                "b.txt".to_string(),
                "c.txt".to_string(),
            ],
        ) {
            SafetyResult::TrashRequested(paths) => {
                assert_eq!(paths, vec!["a.txt", "b.txt", "c.txt"])
            }
            other => panic!("Expected TrashRequested, got {:?}", other),
        }
    }

    #[test]
    fn test_check_rm_no_files() {
        let _guard = EnvGuard::new();
        std::env::set_var("RTK_SAFE_COMMANDS", "1");
        match check("rm", &["-rf".to_string()]) {
            SafetyResult::TrashRequested(paths) => assert!(paths.is_empty()),
            other => panic!("Expected TrashRequested, got {:?}", other),
        }
    }

    // === cat/sed/head (RTK_BLOCK_TOKEN_WASTE) ===

    #[test]
    fn test_check_cat_safe_by_default() {
        let _guard = EnvGuard::new();
        // OPT-IN: with no env var set, cat should pass through unchanged.
        assert_eq!(
            check("cat", &["file.txt".to_string()]),
            SafetyResult::Safe,
            "cat must be passthrough when RTK_BLOCK_TOKEN_WASTE is unset (opt-in default)"
        );
    }

    #[test]
    fn test_check_cat_blocked_when_env_set() {
        let _guard = EnvGuard::new();
        std::env::set_var("RTK_BLOCK_TOKEN_WASTE", "1");
        match check("cat", &["file.txt".to_string()]) {
            SafetyResult::Blocked(msg) => assert!(
                msg.contains("file-reading") || msg.contains("Read"),
                "msg: {msg}"
            ),
            other => panic!("Expected Blocked, got {:?}", other),
        }
    }

    #[test]
    fn test_check_cat_safe_when_explicitly_disabled() {
        let _guard = EnvGuard::new();
        std::env::set_var("RTK_BLOCK_TOKEN_WASTE", "0");
        assert_eq!(check("cat", &["file.txt".to_string()]), SafetyResult::Safe);
    }

    #[test]
    fn test_check_sed_blocked_when_env_set() {
        let _guard = EnvGuard::new();
        std::env::set_var("RTK_BLOCK_TOKEN_WASTE", "1");
        match check("sed", &["-i".to_string(), "s/old/new/g".to_string()]) {
            SafetyResult::Blocked(msg) => assert!(
                msg.contains("file-editing") || msg.contains("Edit"),
                "msg: {msg}"
            ),
            other => panic!("Expected Blocked, got {:?}", other),
        }
    }

    #[test]
    fn test_check_head_blocked_when_env_set() {
        let _guard = EnvGuard::new();
        std::env::set_var("RTK_BLOCK_TOKEN_WASTE", "1");
        match check(
            "head",
            &["-n".to_string(), "10".to_string(), "file.txt".to_string()],
        ) {
            SafetyResult::Blocked(msg) => assert!(
                msg.contains("file-reading") || msg.contains("Read"),
                "msg: {msg}"
            ),
            other => panic!("Expected Blocked, got {:?}", other),
        }
    }

    // === git safety ===

    #[test]
    fn test_check_git_reset_hard_no_panic() {
        let _guard = EnvGuard::new();
        std::env::set_var("RTK_SAFE_COMMANDS", "1");
        // Trigger depends on git state (has_unstaged_changes predicate). Just
        // ensure no panic — actual result is environment-dependent.
        let _ = check("git", &["reset".to_string(), "--hard".to_string()]);
    }

    #[test]
    fn test_check_git_clean_fd_rewritten_when_env_set() {
        let _guard = EnvGuard::new();
        std::env::set_var("RTK_SAFE_COMMANDS", "1");
        match check("git", &["clean".to_string(), "-fd".to_string()]) {
            SafetyResult::Rewritten(cmd) => {
                assert!(cmd.contains("stash -u"));
                assert!(cmd.contains("clean"));
            }
            other => panic!("Expected Rewritten, got {:?}", other),
        }
    }

    #[test]
    fn test_check_git_clean_safe_by_default() {
        let _guard = EnvGuard::new();
        assert_eq!(
            check("git", &["clean".to_string(), "-fd".to_string()]),
            SafetyResult::Safe,
            "git clean must passthrough by default"
        );
    }

    #[test]
    fn test_check_git_clean_safe_when_explicitly_disabled() {
        let _guard = EnvGuard::new();
        std::env::set_var("RTK_SAFE_COMMANDS", "0");
        assert_eq!(
            check("git", &["clean".to_string(), "-fd".to_string()]),
            SafetyResult::Safe
        );
    }

    // === check_raw tests ===

    #[test]
    fn test_check_raw_rm_safe_by_default() {
        let _guard = EnvGuard::new();
        assert_eq!(check_raw("rm file.txt"), SafetyResult::Safe);
    }

    #[test]
    fn test_check_raw_rm_blocked_when_env_set() {
        let _guard = EnvGuard::new();
        std::env::set_var("RTK_SAFE_COMMANDS", "1");
        match check_raw("rm file.txt") {
            SafetyResult::Blocked(_) => {}
            other => panic!("Expected Blocked, got {:?}", other),
        }
    }

    #[test]
    fn test_check_raw_sudo_rm_detected() {
        let _guard = EnvGuard::new();
        std::env::set_var("RTK_SAFE_COMMANDS", "1");
        match check_raw("sudo rm file.txt") {
            SafetyResult::Blocked(_) => {}
            other => panic!("Expected Blocked for sudo rm, got {:?}", other),
        }
    }

    #[test]
    fn test_check_raw_sudo_flags_rm_detected() {
        let _guard = EnvGuard::new();
        std::env::set_var("RTK_SAFE_COMMANDS", "1");
        match check_raw("sudo -u root rm file.txt") {
            SafetyResult::Blocked(_) => {}
            other => panic!("Expected Blocked for sudo -u root rm, got {:?}", other),
        }
    }

    #[test]
    fn test_check_raw_safe_command() {
        let _guard = EnvGuard::new();
        assert_eq!(check_raw("ls -la"), SafetyResult::Safe);
    }

    // === new git safety ===

    #[test]
    fn test_git_checkout_dot_no_panic() {
        let _guard = EnvGuard::new();
        std::env::set_var("RTK_SAFE_COMMANDS", "1");
        // Predicate-gated; just ensure no panic.
        let _ = check("git", &["checkout".to_string(), ".".to_string()]);
    }

    #[test]
    fn test_git_checkout_dashdash_no_panic() {
        let _guard = EnvGuard::new();
        std::env::set_var("RTK_SAFE_COMMANDS", "1");
        let _ = check(
            "git",
            &[
                "checkout".to_string(),
                "--".to_string(),
                "file.txt".to_string(),
            ],
        );
    }

    #[test]
    fn test_git_stash_drop_rewritten_when_env_set() {
        let _guard = EnvGuard::new();
        std::env::set_var("RTK_SAFE_COMMANDS", "1");
        match check("git", &["stash".to_string(), "drop".to_string()]) {
            SafetyResult::Rewritten(cmd) => assert!(cmd.contains("stash pop")),
            other => panic!("Expected Rewritten to stash pop, got {:?}", other),
        }
    }

    #[test]
    fn test_git_clean_f_rewritten_when_env_set() {
        let _guard = EnvGuard::new();
        std::env::set_var("RTK_SAFE_COMMANDS", "1");
        match check("git", &["clean".to_string(), "-f".to_string()]) {
            SafetyResult::Rewritten(cmd) => {
                assert!(cmd.contains("stash -u"));
                assert!(cmd.contains("clean"));
            }
            other => panic!("Expected Rewritten with stash -u, got {:?}", other),
        }
    }

    #[test]
    fn test_git_branch_checkout_safe() {
        let _guard = EnvGuard::new();
        std::env::set_var("RTK_SAFE_COMMANDS", "1");
        // `git checkout <branch>` must NOT match `git checkout .` or
        // `git checkout --`.
        assert_eq!(
            check("git", &["checkout".to_string(), "main".to_string()]),
            SafetyResult::Safe
        );
    }

    #[test]
    fn test_git_checkout_new_branch_safe() {
        let _guard = EnvGuard::new();
        std::env::set_var("RTK_SAFE_COMMANDS", "1");
        assert_eq!(
            check(
                "git",
                &[
                    "checkout".to_string(),
                    "-b".to_string(),
                    "feature".to_string(),
                ],
            ),
            SafetyResult::Safe
        );
    }

    // === false positives ===

    #[test]
    fn test_no_false_positive_catalog() {
        let _guard = EnvGuard::new();
        std::env::set_var("RTK_BLOCK_TOKEN_WASTE", "1");
        assert_eq!(
            check("catalog", &["show".to_string()]),
            SafetyResult::Safe,
            "catalog must not match cat rule"
        );
    }

    #[test]
    fn test_no_false_positive_sedan() {
        let _guard = EnvGuard::new();
        std::env::set_var("RTK_BLOCK_TOKEN_WASTE", "1");
        assert_eq!(
            check("sedan", &[]),
            SafetyResult::Safe,
            "sedan must not match sed rule"
        );
    }

    #[test]
    fn test_no_false_positive_headless() {
        let _guard = EnvGuard::new();
        std::env::set_var("RTK_BLOCK_TOKEN_WASTE", "1");
        assert_eq!(
            check("headless", &["chrome".to_string()]),
            SafetyResult::Safe,
            "headless must not match head rule"
        );
    }

    #[test]
    fn test_no_false_positive_rmdir() {
        let _guard = EnvGuard::new();
        std::env::set_var("RTK_SAFE_COMMANDS", "1");
        assert_eq!(
            check("rmdir", &["empty_dir".to_string()]),
            SafetyResult::Safe,
            "rmdir must not match rm rule"
        );
    }

    #[test]
    fn test_check_raw_no_false_positive_trim() {
        let _guard = EnvGuard::new();
        std::env::set_var("RTK_SAFE_COMMANDS", "1");
        assert_eq!(
            check_raw("trim file.txt"),
            SafetyResult::Safe,
            "trim must not match rm pattern"
        );
    }

    #[test]
    fn test_check_raw_no_false_positive_farm() {
        let _guard = EnvGuard::new();
        std::env::set_var("RTK_SAFE_COMMANDS", "1");
        assert_eq!(
            check_raw("farm --harvest"),
            SafetyResult::Safe,
            "farm must not match rm pattern"
        );
    }

    // === rule parsing ===

    #[test]
    fn test_parse_rule_valid() {
        let content = "---\nname: test-rule\npatterns: [rm]\naction: trash\n---\nSafety message.";
        let rule = parse_rule(content, "test").unwrap();
        assert_eq!(rule.name, "test-rule");
        assert_eq!(rule.patterns, vec!["rm"]);
        assert_eq!(rule.action, "trash");
        assert_eq!(rule.message, "Safety message.");
        assert_eq!(rule.source, "test");
    }

    #[test]
    fn test_parse_rule_no_frontmatter() {
        assert!(parse_rule("No frontmatter here", "test").is_err());
    }

    #[test]
    fn test_parse_rule_defaults() {
        let content = "---\nname: minimal\n---\n";
        let rule = parse_rule(content, "test").unwrap();
        assert_eq!(rule.action, "block");
        assert_eq!(rule.when, "always");
        assert!(rule.enabled);
        assert!(rule.patterns.is_empty());
    }

    #[test]
    fn test_load_all_includes_eleven_builtins() {
        let rules = load_all();
        assert_eq!(
            rules.len(),
            11,
            "Should have exactly 11 built-in rules, got {}",
            rules.len()
        );
        let names: Vec<&str> = rules.iter().map(|r| r.name.as_str()).collect();
        for expected in [
            "rm-to-trash",
            "git-reset-hard",
            "git-checkout-dashdash",
            "git-checkout-dot",
            "git-stash-drop",
            "git-clean-fd",
            "git-clean-df",
            "git-clean-f",
            "block-cat",
            "block-sed",
            "block-head",
        ] {
            assert!(
                names.contains(&expected),
                "missing built-in rule: {expected}"
            );
        }
    }

    #[test]
    fn test_all_builtin_rules_parse_successfully() {
        for (i, content) in DEFAULT_RULES.iter().enumerate() {
            let rule = parse_rule(content, "builtin")
                .unwrap_or_else(|e| panic!("Built-in rule #{i} failed to parse: {e}"));
            assert!(!rule.name.is_empty(), "Rule #{i} has empty name");
            assert!(rule.enabled, "Rule #{i} ({}) should be enabled", rule.name);
            assert!(
                !rule.patterns.is_empty(),
                "Rule '{}' has no patterns",
                rule.name
            );
        }
    }

    // === global-option stripping ===

    #[test]
    fn test_strip_global_options() {
        let cases: &[(&str, &str)] = &[
            ("git --no-pager status", "git status"),
            ("git -C /path/to/project status", "git status"),
            ("git -c core.autocrlf=true diff", "git diff"),
            ("git --git-dir=/path/.git status", "git status"),
            ("git --no-optional-locks status", "git status"),
            ("git --bare log --oneline", "git log --oneline"),
            ("git --literal-pathspecs add .", "git add ."),
            (
                "git -C /path --no-pager --no-optional-locks reset --hard",
                "git reset --hard",
            ),
            ("git reset --hard HEAD~1", "git reset --hard HEAD~1"),
            ("git checkout --force main", "git checkout --force main"),
            ("git status", "git status"),
            ("git log --oneline -10", "git log --oneline -10"),
            ("cargo +nightly test", "cargo test"),
            ("cargo +stable build --release", "cargo build --release"),
            ("cargo test", "cargo test"),
            ("docker --context prod ps", "docker ps"),
            ("docker -H tcp://host:2375 images", "docker images"),
            ("docker --config /tmp/.docker run hello", "docker run hello"),
            ("docker ps", "docker ps"),
            ("kubectl -n kube-system get pods", "kubectl get pods"),
            (
                "kubectl --context prod --namespace default describe pod foo",
                "kubectl describe pod foo",
            ),
            ("kubectl --kubeconfig=/path get svc", "kubectl get svc"),
            ("kubectl get pods", "kubectl get pods"),
            ("rm -rf /tmp/foo", "rm -rf /tmp/foo"),
            ("cat file.txt", "cat file.txt"),
            ("echo hello", "echo hello"),
        ];
        for (input, expected) in cases {
            assert_eq!(
                strip_global_options(input),
                *expected,
                "strip_global_options({input:?})"
            );
        }
    }

    #[test]
    fn test_matches_rule_with_global_options() {
        let cases: &[(&str, &str, bool)] = &[
            ("git reset --hard", "git --no-pager reset --hard HEAD", true),
            ("git reset --hard", "git -C /path reset --hard", true),
            (
                "git reset --hard",
                "git -C /p --no-pager --no-optional-locks reset --hard",
                true,
            ),
            ("git checkout .", "git -C /project checkout .", true),
            (
                "git checkout --",
                "git --no-pager checkout -- file.txt",
                true,
            ),
            (
                "git clean -fd",
                "git -C /path --no-pager --no-optional-locks clean -fd",
                true,
            ),
            ("git stash drop", "git --no-pager stash drop", true),
            ("git reset --hard", "git reset --hard HEAD~1", true),
            ("git checkout .", "git checkout .", true),
            ("git reset --hard", "git reset --soft HEAD", false),
            ("git checkout .", "git checkout main", false),
        ];
        for (pattern, full_cmd, expected) in cases {
            let yaml = format!("---\nname: test\npatterns: [\"{pattern}\"]\n---\n");
            let rule = parse_rule(&yaml, "test").unwrap();
            let binary = full_cmd.split_whitespace().next();
            assert_eq!(
                matches_rule(&rule, binary, full_cmd),
                *expected,
                "matches_rule(pat={pattern:?}, cmd={full_cmd:?})"
            );
        }
    }

    #[test]
    fn test_matches_rule_empty_patterns() {
        let content = "---\nname: no-patterns\n---\n";
        let rule = parse_rule(content, "test").unwrap();
        assert!(!matches_rule(&rule, Some("rm"), "rm file"));
        assert!(!matches_rule(&rule, None, "rm file"));
    }

    #[test]
    fn test_should_apply_env_var_opt_in() {
        let _guard = EnvGuard::new();
        let content = "---\nname: test\npatterns: [rm]\nenv_var: RTK_TEST_VAR\n---\n";
        let rule = parse_rule(content, "test").unwrap();

        // Unset → does NOT apply (opt-IN)
        std::env::remove_var("RTK_TEST_VAR");
        assert!(
            !rule.should_apply(),
            "opt-in: unset env var must mean rule does not apply"
        );

        // "0" / "false" → disabled
        std::env::set_var("RTK_TEST_VAR", "0");
        assert!(!rule.should_apply());
        std::env::set_var("RTK_TEST_VAR", "false");
        assert!(!rule.should_apply());

        // "1" → enabled
        std::env::set_var("RTK_TEST_VAR", "1");
        assert!(rule.should_apply());

        std::env::remove_var("RTK_TEST_VAR");
    }

    #[test]
    fn test_check_when_always() {
        assert!(check_when("always"));
        assert!(check_when(""));
    }

    #[test]
    fn test_check_when_bash_fallback() {
        assert!(check_when("true"));
        assert!(!check_when("false"));
    }
}
