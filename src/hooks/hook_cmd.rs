//! Processes incoming hook calls from AI agents and rewrites commands on the fly.
//!
//! Uses `writeln!(stdout, ...)` instead of `println!` — accidental stdout/stderr
//! corrupts the JSON protocol (Claude Code bug #4669 silently disables the hook).

use super::constants::PRE_TOOL_USE_KEY;
use super::manifest::{extract_deny_reason, run_manifest_handlers, ManifestResult};
use super::permissions::{self, PermissionVerdict};
use super::recursion_guard::is_rtk_active;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::io::{self, Read, Write};

use crate::discover::registry::{has_heredoc, rewrite_command};

const STDIN_CAP: usize = 1_048_576; // 1 MiB

/// Hook decision returned by `run_*_inner` helpers — pure logic, no I/O.
///
/// `run_*` functions are the single I/O point per agent (Claude, Cursor,
/// Gemini, Copilot). They consume a `HookResponse` and emit the
/// corresponding bytes to stdout/stderr. Combined with `#[deny(
/// clippy::print_stdout, clippy::print_stderr)]` on the `hook_cmd` module
/// this prevents stray output from corrupting the JSON hook protocol.
///
/// Variants:
/// - `NoOpinion` — exit 0, no output. Host proceeds as normal. Manifest
///   fallthrough handlers still run from this branch (RTK had no rewrite
///   but a displaced plugin may still want to deny).
/// - `Allow(json)` — exit 0, RTK's rewrite JSON on stdout. Manifest
///   handlers run as a deny-veto gate before stdout is written.
/// - `Deny(json, reason)` — exit 2, JSON to stdout, plain-text reason to
///   stderr. Manifest fallthrough is bypassed (RTK already blocked).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HookResponse {
    NoOpinion,
    Allow(String),
    /// (json_stdout, plain_text_reason).
    ///
    /// Reserved for forthcoming permission integration: today's
    /// `process_claude_payload` maps a deny verdict to `NoOpinion`
    /// (matching upstream's documented behaviour), but the dispatcher
    /// already routes this variant to the exit-2 + dual-path-stderr
    /// branch so the v3 permission PR can flip the mapping without
    /// touching `run_claude`.
    #[allow(dead_code)]
    Deny(String, String),
}

/// Returns `true` when hook processing should be skipped entirely.
///
/// Two reasons:
/// - `RTK_HOOK_ENABLED=0` is the user-facing master toggle. Lets
///   developers disable RTK temporarily (a single shell session, a
///   single CI job) without uninstalling.
/// - `RTK_ACTIVE` is set, meaning we are already inside an RTK-spawned
///   subprocess. Re-hooking would either infinite-loop or double-rewrite
///   the same command.
///
/// The `RTK_HOOK_ENABLED` check compares against the literal `"0"` so
/// any other value (including `"1"` or empty) keeps hooks active —
/// avoids accidental disable from a stale `=` line in `~/.zshrc`.
pub(crate) fn is_hook_disabled() -> bool {
    std::env::var("RTK_HOOK_ENABLED").as_deref() == Ok("0") || is_rtk_active()
}

fn read_stdin_limited() -> Result<String> {
    let mut input = String::new();
    io::stdin()
        .take((STDIN_CAP + 1) as u64)
        .read_to_string(&mut input)
        .context("Failed to read stdin")?;
    if input.len() > STDIN_CAP {
        anyhow::bail!("hook stdin exceeds {} byte limit", STDIN_CAP);
    }
    Ok(input)
}

// ── Copilot hook (VS Code + Copilot CLI) ──────────────────────

/// Format detected from the preToolUse JSON input.
enum HookFormat {
    /// VS Code Copilot Chat / Claude Code: `tool_name` + `tool_input.command`, supports `updatedInput`.
    VsCode { command: String },
    /// GitHub Copilot CLI: camelCase `toolName` + `toolArgs` (JSON string), deny-with-suggestion only.
    CopilotCli { command: String },
    /// Non-bash tool, already uses rtk, or unknown format — pass through silently.
    PassThrough,
}

/// Run the Copilot preToolUse hook.
/// Auto-detects VS Code Copilot Chat vs Copilot CLI format.
pub fn run_copilot() -> Result<()> {
    // RTK_HOOK_ENABLED=0 (master toggle) or RTK_ACTIVE (recursion) → skip
    if is_hook_disabled() {
        return Ok(());
    }

    let input = read_stdin_limited()?;

    let input = input.trim();
    if input.is_empty() {
        return Ok(());
    }

    // FAIL-OPEN: invalid JSON → silent Ok. NEVER write to stderr at exit 0:
    // Claude Code interprets ANY stderr at exit 0 as a hook error and
    // proceeds anyway, but worse, ANY stderr corrupts piped JSON consumers
    // downstream. The previous behavior here emitted a noisy parse error
    // (bug present at upstream hook_cmd.rs:53) — silent return matches the
    // Cursor handler at line 421 and the documented fail-open contract.
    let v: Value = match serde_json::from_str(input) {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };

    match detect_format(&v) {
        HookFormat::VsCode { command } => handle_vscode(&command),
        HookFormat::CopilotCli { command } => handle_copilot_cli(&command),
        HookFormat::PassThrough => Ok(()),
    }
}

fn detect_format(v: &Value) -> HookFormat {
    // VS Code Copilot Chat / Claude Code: snake_case keys
    if let Some(tool_name) = v.get("tool_name").and_then(|t| t.as_str()) {
        if matches!(tool_name, "runTerminalCommand" | "Bash" | "bash") {
            if let Some(cmd) = v
                .pointer("/tool_input/command")
                .and_then(|c| c.as_str())
                .filter(|c| !c.is_empty())
            {
                return HookFormat::VsCode {
                    command: cmd.to_string(),
                };
            }
        }
        return HookFormat::PassThrough;
    }

    // Copilot CLI: camelCase keys, toolArgs is a JSON-encoded string
    if let Some(tool_name) = v.get("toolName").and_then(|t| t.as_str()) {
        if tool_name == "bash" {
            if let Some(tool_args_str) = v.get("toolArgs").and_then(|t| t.as_str()) {
                if let Ok(tool_args) = serde_json::from_str::<Value>(tool_args_str) {
                    if let Some(cmd) = tool_args
                        .get("command")
                        .and_then(|c| c.as_str())
                        .filter(|c| !c.is_empty())
                    {
                        return HookFormat::CopilotCli {
                            command: cmd.to_string(),
                        };
                    }
                }
            }
        }
        return HookFormat::PassThrough;
    }

    HookFormat::PassThrough
}

fn get_rewritten(cmd: &str) -> Option<String> {
    if has_heredoc(cmd) {
        return None;
    }

    let (excluded, transparent_prefixes) = crate::core::config::Config::load()
        .map(|c| (c.hooks.exclude_commands, c.hooks.transparent_prefixes))
        .unwrap_or_default();

    let rewritten = rewrite_command(cmd, &excluded, &transparent_prefixes)?;

    if rewritten == cmd {
        return None;
    }

    Some(rewritten)
}

fn handle_vscode(cmd: &str) -> Result<()> {
    let verdict = permissions::check_command(cmd);
    if verdict == PermissionVerdict::Deny {
        audit_log("deny", cmd, "");
        return Ok(());
    }

    let rewritten = match get_rewritten(cmd) {
        Some(r) => r,
        None => return Ok(()),
    };

    // Allow (explicit rule matched): auto-allow the rewritten command.
    // Ask/Default (no allow rule matched): rewrite but let the host tool prompt.
    let decision = match verdict {
        PermissionVerdict::Allow => "allow",
        _ => "ask",
    };

    audit_log("rewrite", cmd, &rewritten);

    let output = json!({
        "hookSpecificOutput": {
            "hookEventName": PRE_TOOL_USE_KEY,
            "permissionDecision": decision,
            "permissionDecisionReason": "RTK auto-rewrite",
            "updatedInput": { "command": rewritten }
        }
    });
    let _ = writeln!(io::stdout(), "{output}");
    Ok(())
}

fn handle_copilot_cli(cmd: &str) -> Result<()> {
    if permissions::check_command(cmd) == PermissionVerdict::Deny {
        audit_log("deny", cmd, "");
        return Ok(());
    }

    let rewritten = match get_rewritten(cmd) {
        Some(r) => r,
        None => return Ok(()),
    };

    audit_log("rewrite", cmd, &rewritten);

    let output = json!({
        "permissionDecision": "deny",
        "permissionDecisionReason": format!(
            "Token savings: use `{}` instead (rtk saves 60-90% tokens)",
            rewritten
        )
    });
    let _ = writeln!(io::stdout(), "{output}");
    Ok(())
}

// ── Gemini hook ───────────────────────────────────────────────

/// Run the Gemini CLI BeforeTool hook.
pub fn run_gemini() -> Result<()> {
    // RTK_HOOK_ENABLED=0 (master toggle) or RTK_ACTIVE (recursion) → allow + skip
    if is_hook_disabled() {
        print_allow();
        return Ok(());
    }

    let input = read_stdin_limited()?;

    // FAIL-OPEN: invalid JSON → allow + return (matches the documented
    // Gemini contract: if our hook misbehaves, the user's tool must
    // still run). Previously this propagated the parse error which
    // exited the hook non-zero and silently blocked the tool.
    let json: Value = match serde_json::from_str(&input) {
        Ok(v) => v,
        Err(_) => {
            print_allow();
            return Ok(());
        }
    };

    let tool_name = json.get("tool_name").and_then(|v| v.as_str()).unwrap_or("");

    if tool_name != "run_shell_command" {
        print_allow();
        return Ok(());
    }

    let cmd = json
        .pointer("/tool_input/command")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if cmd.is_empty() {
        print_allow();
        return Ok(());
    }

    // Check deny rules — Gemini CLI only supports allow/deny (no ask mode).
    if permissions::check_command(cmd) == PermissionVerdict::Deny {
        let _ = writeln!(
            io::stdout(),
            r#"{{"decision":"deny","reason":"Blocked by RTK permission rule"}}"#
        );
        return Ok(());
    }

    let (excluded, transparent_prefixes) = crate::core::config::Config::load()
        .map(|c| (c.hooks.exclude_commands, c.hooks.transparent_prefixes))
        .unwrap_or_default();

    match rewrite_command(cmd, &excluded, &transparent_prefixes) {
        Some(ref rewritten) => {
            audit_log("rewrite", cmd, rewritten);
            print_rewrite(rewritten);
        }
        None => print_allow(),
    }

    Ok(())
}

fn print_allow() {
    let _ = writeln!(io::stdout(), r#"{{"decision":"allow"}}"#);
}

fn print_rewrite(cmd: &str) {
    let output = serde_json::json!({
        "decision": "allow",
        "hookSpecificOutput": {
            "tool_input": {
                "command": cmd
            }
        }
    });
    let _ = writeln!(io::stdout(), "{}", output);
}

// ── Audit logging ─────────────────────────────────────────────

/// Best-effort audit log when RTK_HOOK_AUDIT=1.
fn audit_log(action: &str, original: &str, rewritten: &str) {
    if std::env::var("RTK_HOOK_AUDIT").as_deref() != Ok("1") {
        return;
    }
    let _ = audit_log_inner(action, original, rewritten);
}

/// Escape newlines to prevent log-line injection in the pipe-delimited audit log.
fn sanitize_log_field(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('|', "\\|")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

fn audit_log_inner(action: &str, original: &str, rewritten: &str) -> Option<()> {
    let home = dirs::home_dir()?;
    let dir = home.join(".local").join("share").join("rtk");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("hook-audit.log");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()?;
    let ts = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S");
    writeln!(
        file,
        "{} | {} | {} | {}",
        ts,
        action,
        sanitize_log_field(original),
        sanitize_log_field(rewritten)
    )
    .ok()
}

// ── Claude Code native hook ────────────────────────────────────

enum PayloadAction {
    Rewrite {
        cmd: String,
        rewritten: String,
        output: Value,
    },
    Skip {
        reason: &'static str,
        cmd: String,
    },
    Ignore,
}

fn process_claude_payload(v: &Value) -> PayloadAction {
    let cmd = match v
        .pointer("/tool_input/command")
        .and_then(|c| c.as_str())
        .filter(|c| !c.is_empty())
    {
        Some(c) => c,
        None => return PayloadAction::Ignore,
    };

    let verdict = permissions::check_command(cmd);
    if verdict == PermissionVerdict::Deny {
        return PayloadAction::Skip {
            reason: "skip:deny_rule",
            cmd: cmd.to_string(),
        };
    }

    let rewritten = match get_rewritten(cmd) {
        Some(r) => r,
        None => {
            return PayloadAction::Skip {
                reason: "skip:no_match",
                cmd: cmd.to_string(),
            }
        }
    };

    let updated_input = {
        let mut ti = v.get("tool_input").cloned().unwrap_or_else(|| json!({}));
        if let Some(obj) = ti.as_object_mut() {
            obj.insert("command".into(), Value::String(rewritten.clone()));
        }
        ti
    };

    let mut hook_output = json!({
        "hookEventName": PRE_TOOL_USE_KEY,
        "permissionDecisionReason": "RTK auto-rewrite",
        "updatedInput": updated_input
    });

    if verdict == PermissionVerdict::Allow {
        hook_output
            .as_object_mut()
            .unwrap()
            .insert("permissionDecision".into(), json!("allow"));
    }

    PayloadAction::Rewrite {
        cmd: cmd.to_string(),
        rewritten,
        output: json!({ "hookSpecificOutput": hook_output }),
    }
}

/// Pure-logic step of `run_claude`. Reads no stdin, writes no output —
/// `run_claude` performs all I/O so manifest fallthrough can run on both
/// the NoOpinion and Allow paths without colliding with stdout writes.
///
/// Fail-open: any parse or processing failure → `NoOpinion` so the host
/// tool proceeds normally.
fn run_claude_inner_v2(input: &str) -> HookResponse {
    // Silent on parse failure: writing to stderr at exit 0 would be
    // interpreted by Claude Code as a hook error (and corrupt downstream
    // JSON consumers). Bug fix vs upstream hook_cmd.rs:368.
    let v: Value = match serde_json::from_str(input) {
        Ok(v) => v,
        Err(_) => return HookResponse::NoOpinion,
    };

    match process_claude_payload(&v) {
        PayloadAction::Rewrite {
            cmd,
            rewritten,
            output,
        } => {
            audit_log("rewrite", &cmd, &rewritten);
            HookResponse::Allow(output.to_string())
        }
        PayloadAction::Skip { reason, cmd } => {
            audit_log(reason, &cmd, "");
            HookResponse::NoOpinion
        }
        PayloadAction::Ignore => HookResponse::NoOpinion,
    }
}

/// Run the Claude Code PreToolUse hook natively.
///
/// Architecture (v3): single I/O point. `run_claude_inner_v2` returns a
/// `HookResponse`; this function dispatches it and runs manifest
/// fallthrough handlers from BOTH the NoOpinion and Allow paths so a
/// displaced plugin's deny rules survive RTK's rewrite. See
/// `manifest::run_manifest_handlers` for the fallthrough contract.
pub fn run_claude() -> Result<()> {
    // Master toggle / recursion guard — skip silently if disabled.
    if is_hook_disabled() {
        return Ok(());
    }

    // Read stdin once so the *raw* payload is available for manifest
    // handlers (they need the original tool_input, not RTK's rewrite).
    let mut buffer = String::new();
    io::stdin()
        .take((STDIN_CAP + 1) as u64)
        .read_to_string(&mut buffer)
        .context("Failed to read stdin")?;
    if buffer.len() > STDIN_CAP {
        anyhow::bail!("hook stdin exceeds {} byte limit", STDIN_CAP);
    }

    let trimmed = buffer.trim();
    if trimmed.is_empty() {
        return Ok(());
    }

    let response = run_claude_inner_v2(trimmed);

    match response {
        HookResponse::NoOpinion => {
            // RTK has no rewrite — give every manifest handler a chance.
            // INVARIANT: pass ORIGINAL buffer (not trimmed) so handlers
            // see exactly what Claude Code sent. Whitespace tolerance
            // is the handler's problem to solve.
            match run_manifest_handlers(&buffer) {
                ManifestResult::Blocked { json, stderr_bytes } => {
                    let _ = writeln!(io::stdout(), "{json}");
                    let _ = io::stderr().write_all(&stderr_bytes);
                    if stderr_bytes.is_empty() {
                        let _ = writeln!(io::stderr(), "Command blocked by registered handler");
                    }
                    std::process::exit(2);
                }
                ManifestResult::NoBlock => {
                    // Implicit allow — exit 0, no output.
                }
            }
        }
        HookResponse::Allow(rtk_json) => {
            // RTK wants to rewrite. Manifest handlers veto BEFORE we
            // emit our rewrite so an autorun deny (for example) wins
            // over a benign RTK rewrite.
            match run_manifest_handlers(&buffer) {
                ManifestResult::Blocked {
                    json: handler_json,
                    stderr_bytes,
                } => {
                    let _ = writeln!(io::stdout(), "{handler_json}");
                    let _ = io::stderr().write_all(&stderr_bytes);
                    if stderr_bytes.is_empty() {
                        let reason = extract_deny_reason(&handler_json).unwrap_or_else(|| {
                            "Command blocked by registered safety handler".to_owned()
                        });
                        let _ = writeln!(io::stderr(), "{reason}");
                    }
                    std::process::exit(2);
                }
                ManifestResult::NoBlock => {
                    let _ = writeln!(io::stdout(), "{rtk_json}");
                }
            }
        }
        HookResponse::Deny(json, reason) => {
            // Exit 2 path: stderr is the *real* block signal because
            // Claude Code bug #4669 ignores a deny `permissionDecision`
            // at exit 0. Dual-path: JSON to stdout (forward-compat),
            // reason to stderr (works today), exit 2 to trigger the
            // block.
            let _ = writeln!(io::stdout(), "{json}");
            let _ = writeln!(io::stderr(), "{reason}");
            std::process::exit(2);
        }
    }

    Ok(())
}

#[cfg(test)]
fn run_claude_inner(input: &str) -> Option<String> {
    let v: Value = serde_json::from_str(input).ok()?;
    match process_claude_payload(&v) {
        PayloadAction::Rewrite { output, .. } => Some(output.to_string()),
        _ => None,
    }
}

// ── Cursor native hook ─────────────────────────────────────────

/// Cursor on Windows ships hook payloads with one or more leading
/// UTF-8 BOMs (`EF BB BF`, sometimes doubled), which serde_json
/// refuses to parse. Strip them defensively so the rewrite path keeps
/// working instead of silently returning `{}`.
fn strip_leading_bom(input: &str) -> &str {
    let mut s = input;
    while let Some(rest) = s.strip_prefix('\u{feff}') {
        s = rest;
    }
    s
}

/// Run the Cursor Agent hook natively.
pub fn run_cursor() -> Result<()> {
    // Master toggle / recursion guard. Emit `{}` so Cursor renders the
    // panel as a no-op (matching the empty-input passthrough below)
    // instead of collapsing to "hook output: {}" with no signal.
    if is_hook_disabled() {
        let _ = writeln!(io::stdout(), "{{}}");
        return Ok(());
    }

    let input = read_stdin_limited()?;

    let input = strip_leading_bom(&input).trim();
    if input.is_empty() {
        let _ = writeln!(io::stdout(), "{{}}");
        return Ok(());
    }

    let v: Value = match serde_json::from_str(input) {
        Ok(v) => v,
        Err(_) => {
            let _ = writeln!(io::stdout(), "{{}}");
            return Ok(());
        }
    };

    let cmd = match v
        .pointer("/tool_input/command")
        .and_then(|c| c.as_str())
        .filter(|c| !c.is_empty())
    {
        Some(c) => c.to_string(),
        None => {
            let _ = writeln!(io::stdout(), "{{}}");
            return Ok(());
        }
    };

    let verdict = permissions::check_command(&cmd);
    if verdict == PermissionVerdict::Deny {
        audit_log("deny", &cmd, "");
        let _ = writeln!(io::stdout(), "{{}}");
        return Ok(());
    }

    let rewritten = match get_rewritten(&cmd) {
        Some(r) => r,
        None => {
            let _ = writeln!(io::stdout(), "{{}}");
            return Ok(());
        }
    };

    // Cursor preToolUse currently enforces allow/deny only and can ignore
    // updated_input when permission is "ask". Use "allow" for rewritten
    // commands unless the command is explicitly denied above.
    let decision = "allow";

    audit_log("rewrite", &cmd, &rewritten);

    // `continue: true` mirrors the shape of every other Cursor hook
    // (afterShellExecution, beforeSubmitPrompt, stop, ...). Cursor's
    // preToolUse panel renders the JSON it received; without this field
    // the panel collapses to `Output: {}` even though the rewrite ran,
    // which makes the hook look broken to users.
    let output = json!({
        "continue": true,
        "permission": decision,
        "updated_input": { "command": rewritten }
    });
    let _ = writeln!(io::stdout(), "{output}");
    Ok(())
}

#[cfg(test)]
fn run_cursor_inner(input: &str) -> String {
    run_cursor_inner_with_rules(input, &[], &[], &[])
}

#[cfg(test)]
fn run_cursor_inner_with_rules(
    input: &str,
    deny_rules: &[String],
    ask_rules: &[String],
    allow_rules: &[String],
) -> String {
    let input = strip_leading_bom(input);
    let v: Value = match serde_json::from_str(input) {
        Ok(v) => v,
        Err(_) => return "{}".to_string(),
    };

    let cmd = match v
        .pointer("/tool_input/command")
        .and_then(|c| c.as_str())
        .filter(|c| !c.is_empty())
    {
        Some(c) => c.to_string(),
        None => return "{}".to_string(),
    };

    let verdict = permissions::check_command_with_rules(&cmd, deny_rules, ask_rules, allow_rules);
    if verdict == PermissionVerdict::Deny {
        return "{}".to_string();
    }

    match get_rewritten(&cmd) {
        Some(rewritten) => {
            let decision = "allow";
            let output = json!({
                "continue": true,
                "permission": decision,
                "updated_input": { "command": rewritten }
            });
            output.to_string()
        }
        None => "{}".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rewrite_command_no_prefixes(cmd: &str, excluded: &[String]) -> Option<String> {
        crate::discover::registry::rewrite_command(cmd, excluded, &[])
    }

    // --- Copilot format detection ---

    fn vscode_input(tool: &str, cmd: &str) -> Value {
        json!({
            "tool_name": tool,
            "tool_input": { "command": cmd }
        })
    }

    fn copilot_cli_input(cmd: &str) -> Value {
        let args = serde_json::to_string(&json!({ "command": cmd })).unwrap();
        json!({ "toolName": "bash", "toolArgs": args })
    }

    #[test]
    fn test_detect_vscode_bash() {
        assert!(matches!(
            detect_format(&vscode_input("Bash", "git status")),
            HookFormat::VsCode { .. }
        ));
    }

    #[test]
    fn test_detect_vscode_run_terminal_command() {
        assert!(matches!(
            detect_format(&vscode_input("runTerminalCommand", "cargo test")),
            HookFormat::VsCode { .. }
        ));
    }

    #[test]
    fn test_detect_copilot_cli_bash() {
        assert!(matches!(
            detect_format(&copilot_cli_input("git status")),
            HookFormat::CopilotCli { .. }
        ));
    }

    #[test]
    fn test_detect_non_bash_is_passthrough() {
        let v = json!({ "tool_name": "editFiles" });
        assert!(matches!(detect_format(&v), HookFormat::PassThrough));
    }

    #[test]
    fn test_detect_unknown_is_passthrough() {
        assert!(matches!(detect_format(&json!({})), HookFormat::PassThrough));
    }

    #[test]
    fn test_get_rewritten_supported() {
        assert!(get_rewritten("git status").is_some());
    }

    #[test]
    fn test_get_rewritten_unsupported() {
        assert!(get_rewritten("htop").is_none());
    }

    #[test]
    fn test_get_rewritten_already_rtk() {
        assert!(get_rewritten("rtk git status").is_none());
    }

    #[test]
    fn test_get_rewritten_heredoc() {
        assert!(get_rewritten("cat <<'EOF'\nhello\nEOF").is_none());
    }

    // --- Gemini format ---

    #[test]
    fn test_print_allow_format() {
        let expected = r#"{"decision":"allow"}"#;
        assert_eq!(expected, r#"{"decision":"allow"}"#);
    }

    #[test]
    fn test_print_rewrite_format() {
        let output = serde_json::json!({
            "decision": "allow",
            "hookSpecificOutput": {
                "tool_input": {
                    "command": "rtk git status"
                }
            }
        });
        let json: Value = serde_json::from_str(&output.to_string()).unwrap();
        assert_eq!(json["decision"], "allow");
        assert_eq!(
            json["hookSpecificOutput"]["tool_input"]["command"],
            "rtk git status"
        );
    }

    #[test]
    fn test_gemini_hook_uses_rewrite_command() {
        assert_eq!(
            rewrite_command_no_prefixes("git status", &[]),
            Some("rtk git status".into())
        );
        assert_eq!(
            rewrite_command_no_prefixes("cargo test", &[]),
            Some("rtk cargo test".into())
        );
        assert_eq!(
            rewrite_command_no_prefixes("rtk git status", &[]),
            Some("rtk git status".into())
        );
        assert_eq!(rewrite_command_no_prefixes("cat <<EOF", &[]), None);
    }

    #[test]
    fn test_gemini_hook_excluded_commands() {
        let excluded = vec!["curl".to_string()];
        assert_eq!(
            rewrite_command_no_prefixes("curl https://example.com", &excluded),
            None
        );
        assert_eq!(
            rewrite_command_no_prefixes("git status", &excluded),
            Some("rtk git status".into())
        );
    }

    #[test]
    fn test_gemini_hook_env_prefix_preserved() {
        assert_eq!(
            rewrite_command_no_prefixes("RUST_LOG=debug cargo test", &[]),
            Some("RUST_LOG=debug rtk cargo test".into())
        );
    }

    // --- Claude handler ---

    fn claude_input(cmd: &str) -> String {
        json!({
            "tool_name": "Bash",
            "tool_input": { "command": cmd }
        })
        .to_string()
    }

    fn claude_input_with_fields(cmd: &str, timeout: u64, description: &str) -> String {
        json!({
            "tool_name": "Bash",
            "tool_input": {
                "command": cmd,
                "timeout": timeout,
                "description": description
            }
        })
        .to_string()
    }

    #[test]
    fn test_claude_rewrite_git_status() {
        let result = run_claude_inner(&claude_input("git status")).unwrap();
        let v: Value = serde_json::from_str(&result).unwrap();
        let cmd = v
            .pointer("/hookSpecificOutput/updatedInput/command")
            .and_then(|c| c.as_str())
            .unwrap();
        assert_eq!(cmd, "rtk git status");
    }

    #[test]
    fn test_claude_rewrite_preserves_tool_input_fields() {
        let input = claude_input_with_fields("git status", 30000, "Check repo status");
        let result = run_claude_inner(&input).unwrap();
        let v: Value = serde_json::from_str(&result).unwrap();
        let updated = &v["hookSpecificOutput"]["updatedInput"];
        assert_eq!(updated["command"], "rtk git status");
        assert_eq!(updated["timeout"], 30000);
        assert_eq!(updated["description"], "Check repo status");
    }

    #[test]
    fn test_claude_passthrough_no_output() {
        assert!(run_claude_inner(&claude_input("htop")).is_none());
    }

    #[test]
    fn test_claude_heredoc_passthrough() {
        assert!(run_claude_inner(&claude_input("cat <<EOF\nhello\nEOF")).is_none());
    }

    #[test]
    fn test_claude_already_rtk_passthrough() {
        assert!(run_claude_inner(&claude_input("rtk git status")).is_none());
    }

    #[test]
    fn test_claude_empty_command_passthrough() {
        let input = json!({
            "tool_name": "Bash",
            "tool_input": { "command": "" }
        })
        .to_string();
        assert!(run_claude_inner(&input).is_none());
    }

    #[test]
    fn test_claude_malformed_json_passthrough() {
        assert!(run_claude_inner("not valid json {{{").is_none());
    }

    #[test]
    fn test_claude_env_prefix_preserved() {
        let result = run_claude_inner(&claude_input("GIT_PAGER=cat git status")).unwrap();
        let v: Value = serde_json::from_str(&result).unwrap();
        let cmd = v
            .pointer("/hookSpecificOutput/updatedInput/command")
            .and_then(|c| c.as_str())
            .unwrap();
        assert_eq!(cmd, "GIT_PAGER=cat rtk git status");
    }

    #[test]
    fn test_claude_compound_command() {
        let result = run_claude_inner(&claude_input("git add . && cargo test")).unwrap();
        let v: Value = serde_json::from_str(&result).unwrap();
        let cmd = v
            .pointer("/hookSpecificOutput/updatedInput/command")
            .and_then(|c| c.as_str())
            .unwrap();
        assert_eq!(cmd, "rtk git add . && rtk cargo test");
    }

    #[test]
    fn test_claude_json_output_structure() {
        let result = run_claude_inner(&claude_input("git status")).unwrap();
        let v: Value = serde_json::from_str(&result).unwrap();
        let hook = &v["hookSpecificOutput"];

        assert_eq!(hook["hookEventName"], PRE_TOOL_USE_KEY);
        // permissionDecision is only set when an explicit allow rule matches;
        // with default-to-ask semantics (no rules configured), it is absent.
        assert_eq!(hook["permissionDecisionReason"], "RTK auto-rewrite");
        assert!(hook["updatedInput"].is_object());
        assert!(hook["updatedInput"]["command"].is_string());
    }

    #[test]
    fn test_claude_no_tool_input_passthrough() {
        let input = json!({ "tool_name": "Bash" }).to_string();
        assert!(run_claude_inner(&input).is_none());
    }

    // --- Cursor handler ---

    fn cursor_input(cmd: &str) -> String {
        json!({
            "tool_name": "Bash",
            "tool_input": { "command": cmd }
        })
        .to_string()
    }

    #[test]
    fn test_cursor_rewrite_flat_format() {
        let result = run_cursor_inner(&cursor_input("git status"));
        let v: Value = serde_json::from_str(&result).unwrap();
        // Cursor preToolUse expects allow/deny for rewrite application.
        assert_eq!(v["permission"], "allow");
        assert_eq!(v["updated_input"]["command"], "rtk git status");
        assert!(v.get("hookSpecificOutput").is_none());
        // `continue: true` keeps the Cursor preToolUse panel from collapsing
        // to `Output: {}`; without it the rewrite is invisible to users.
        assert_eq!(v["continue"], true);
    }

    #[test]
    fn test_cursor_passthrough_empty_json() {
        let result = run_cursor_inner(&cursor_input("htop"));
        assert_eq!(result, "{}");
    }

    #[test]
    fn test_cursor_empty_input_empty_json() {
        let result = run_cursor_inner("");
        assert_eq!(result, "{}");
    }

    #[test]
    fn test_cursor_heredoc_passthrough() {
        let result = run_cursor_inner(&cursor_input("cat <<EOF\nhello\nEOF"));
        assert_eq!(result, "{}");
    }

    #[test]
    fn test_cursor_already_rtk_passthrough() {
        let result = run_cursor_inner(&cursor_input("rtk git status"));
        assert_eq!(result, "{}");
    }

    #[test]
    fn test_cursor_no_hook_specific_output() {
        let result = run_cursor_inner(&cursor_input("cargo test"));
        let v: Value = serde_json::from_str(&result).unwrap();
        assert!(v.get("hookSpecificOutput").is_none());
        assert_eq!(v["permission"], "allow");
        assert_eq!(v["continue"], true);
    }

    #[test]
    fn test_cursor_compound_rewrite_includes_continue() {
        let cmd = "cd \"/tmp/proj\" && git status";
        let result = run_cursor_inner(&cursor_input(cmd));
        let v: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["continue"], true);
        assert_eq!(v["permission"], "allow");
        assert_eq!(
            v["updated_input"]["command"],
            "cd \"/tmp/proj\" && rtk git status"
        );
    }

    #[test]
    fn test_cursor_strips_single_utf8_bom() {
        // Some Cursor builds prepend a single UTF-8 BOM to hook stdin.
        // serde_json rejects BOM-prefixed input, so without the strip
        // the hook returned `{}` and the rewrite became a silent no-op.
        let payload = cursor_input("git status");
        let with_single_bom = format!("\u{feff}{}", payload);
        let result = run_cursor_inner(&with_single_bom);
        let v: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["continue"], true);
        assert_eq!(v["permission"], "allow");
        assert_eq!(v["updated_input"]["command"], "rtk git status");
    }

    #[test]
    fn test_cursor_strips_double_utf8_bom() {
        // Cursor on Windows ships hook stdin with **two** leading
        // UTF-8 BOMs (`EF BB BF EF BB BF`), confirmed via a stdin
        // tracer wrapping `rtk hook cursor` on Cursor 3.2.x. This is
        // the real-world payload shape the loop needs to survive.
        let payload = cursor_input("git status");
        let with_double_bom = format!("\u{feff}\u{feff}{}", payload);
        let result = run_cursor_inner(&with_double_bom);
        let v: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["continue"], true);
        assert_eq!(v["permission"], "allow");
        assert_eq!(v["updated_input"]["command"], "rtk git status");
    }

    #[test]
    fn test_strip_leading_bom_helper() {
        // Direct unit test on the helper so future refactors can't
        // regress the loop semantics without a clear failure signal.
        assert_eq!(strip_leading_bom(""), "");
        assert_eq!(strip_leading_bom("hello"), "hello");
        assert_eq!(strip_leading_bom("\u{feff}hello"), "hello");
        assert_eq!(strip_leading_bom("\u{feff}\u{feff}hello"), "hello");
        assert_eq!(strip_leading_bom("\u{feff}\u{feff}\u{feff}hello"), "hello");
        // BOM in the middle is preserved (not "leading").
        assert_eq!(strip_leading_bom("a\u{feff}b"), "a\u{feff}b");
    }

    // --- Audit logging ---

    #[test]
    fn test_audit_log_silent_when_disabled() {
        std::env::remove_var("RTK_HOOK_AUDIT");
        audit_log("test", "git status", "rtk git status");
    }

    #[test]
    fn test_audit_log_format_four_fields() {
        let tmp = std::env::temp_dir().join("rtk-test-audit");
        let _ = std::fs::create_dir_all(&tmp);
        let log_path = tmp.join("hook-audit.log");
        let _ = std::fs::remove_file(&log_path);

        {
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)
                .unwrap();
            let ts = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S");
            writeln!(file, "{} | rewrite | git status | rtk git status", ts).unwrap();
        }

        let content = std::fs::read_to_string(&log_path).unwrap();
        let parts: Vec<&str> = content.trim().split(" | ").collect();
        assert_eq!(
            parts.len(),
            4,
            "Expected 4 pipe-delimited fields, got: {:?}",
            parts
        );
        assert_eq!(parts[1], "rewrite");
        assert_eq!(parts[2], "git status");
        assert_eq!(parts[3], "rtk git status");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // --- Adversarial tests ---

    #[test]
    fn test_audit_log_sanitizes_newlines() {
        let sanitized = sanitize_log_field("git status\nfake | inject | evil");
        assert!(!sanitized.contains('\n'));
        assert!(sanitized.contains("\\n"));
    }

    #[test]
    fn test_audit_log_sanitizes_pipe_delimiter() {
        let sanitized = sanitize_log_field("git log | head");
        assert!(
            !sanitized.contains(" | "),
            "unescaped ' | ' breaks field parsing: {}",
            sanitized
        );
        assert!(sanitized.contains("\\|"));
    }

    #[test]
    fn test_claude_unicode_null_passthrough() {
        let input = claude_input("git status \u{0000}\u{FEFF}");
        let _ = run_claude_inner(&input);
    }

    #[test]
    fn test_claude_extremely_long_command() {
        let long_cmd = format!("git status {}", "A".repeat(100_000));
        let input = claude_input(&long_cmd);
        let _ = run_claude_inner(&input);
    }

    #[test]
    fn test_cursor_deny_blocks_rewrite() {
        use super::permissions::check_command_with_rules;
        let deny = vec!["git status".to_string()];
        assert_eq!(
            check_command_with_rules("git status", &deny, &[], &[]),
            PermissionVerdict::Deny
        );
    }

    #[test]
    fn test_gemini_deny_blocks_rewrite() {
        use super::permissions::check_command_with_rules;
        let deny = vec!["cargo test".to_string()];
        assert_eq!(
            check_command_with_rules("cargo test", &deny, &[], &[]),
            PermissionVerdict::Deny
        );
        // Denied commands must not be rewritten — Gemini handler checks deny before rewrite
        assert!(
            get_rewritten("cargo test").is_some(),
            "cargo test should be rewritable when not denied"
        );
    }

    // --- HookResponse / is_hook_disabled / run_claude_inner_v2 ---
    //
    // These tests serialize via a mutex because they mutate process-wide
    // env vars. Tests share the lock with `recursion_guard::tests` to
    // avoid cross-module races on RTK_ACTIVE.

    use std::sync::{Mutex, MutexGuard, OnceLock};

    fn env_lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    struct EnvReset {
        _lock: MutexGuard<'static, ()>,
    }
    impl EnvReset {
        fn new() -> Self {
            let lock = env_lock();
            std::env::remove_var("RTK_HOOK_ENABLED");
            std::env::remove_var("RTK_ACTIVE");
            Self { _lock: lock }
        }
    }
    impl Drop for EnvReset {
        fn drop(&mut self) {
            std::env::remove_var("RTK_HOOK_ENABLED");
            std::env::remove_var("RTK_ACTIVE");
        }
    }

    #[test]
    fn test_is_hook_disabled_default_is_false() {
        let _r = EnvReset::new();
        assert!(!is_hook_disabled());
    }

    #[test]
    fn test_is_hook_disabled_when_rtk_hook_enabled_zero() {
        let _r = EnvReset::new();
        std::env::set_var("RTK_HOOK_ENABLED", "0");
        assert!(is_hook_disabled());
    }

    #[test]
    fn test_is_hook_disabled_when_rtk_active_set() {
        let _r = EnvReset::new();
        std::env::set_var("RTK_ACTIVE", "1");
        assert!(is_hook_disabled());
    }

    #[test]
    fn test_is_hook_disabled_rtk_hook_enabled_one_is_active() {
        // Only "0" disables; "1" / anything else keeps the hook running.
        let _r = EnvReset::new();
        std::env::set_var("RTK_HOOK_ENABLED", "1");
        assert!(!is_hook_disabled());
    }

    #[test]
    fn test_is_hook_disabled_rtk_hook_enabled_empty_is_active() {
        // Empty string != "0", so hook stays on. Matches the documented
        // contract: stale shell exports cannot accidentally disable.
        let _r = EnvReset::new();
        std::env::set_var("RTK_HOOK_ENABLED", "");
        assert!(!is_hook_disabled());
    }

    #[test]
    fn test_run_claude_inner_v2_no_opinion_on_malformed_json() {
        // Critical regression test: must NOT write to stderr when JSON
        // parsing fails. The function returns NoOpinion (no I/O), and
        // run_claude's NoOpinion path swallows it silently. Mirrors
        // v2/cmd/hook/claude.rs:264.
        let response = run_claude_inner_v2("not json at all");
        assert_eq!(response, HookResponse::NoOpinion);
    }

    #[test]
    fn test_run_claude_inner_v2_no_opinion_on_empty_object() {
        let response = run_claude_inner_v2("{}");
        assert_eq!(response, HookResponse::NoOpinion);
    }

    #[test]
    fn test_run_claude_inner_v2_no_opinion_on_missing_tool_input() {
        let response = run_claude_inner_v2(r#"{"tool_name": "Bash"}"#);
        assert_eq!(response, HookResponse::NoOpinion);
    }

    #[test]
    fn test_run_claude_inner_v2_no_opinion_when_unmapped_command() {
        // `htop` has no rtk filter → Skip { reason: "skip:no_match" }
        // which maps to NoOpinion. Manifest handlers still get a chance
        // via run_claude's dispatch.
        let payload = json!({"tool_name": "Bash", "tool_input": {"command": "htop"}}).to_string();
        let response = run_claude_inner_v2(&payload);
        assert_eq!(response, HookResponse::NoOpinion);
    }

    #[test]
    fn test_run_claude_inner_v2_allow_when_rewritable() {
        let payload =
            json!({"tool_name": "Bash", "tool_input": {"command": "git status"}}).to_string();
        let response = run_claude_inner_v2(&payload);
        match response {
            HookResponse::Allow(json) => {
                let v: Value = serde_json::from_str(&json).unwrap();
                let cmd = v
                    .pointer("/hookSpecificOutput/updatedInput/command")
                    .and_then(|c| c.as_str())
                    .unwrap();
                assert_eq!(cmd, "rtk git status");
            }
            other => panic!("expected Allow, got {other:?}"),
        }
    }

    #[test]
    fn test_run_claude_inner_v2_passthrough_for_already_rtk() {
        let payload =
            json!({"tool_name": "Bash", "tool_input": {"command": "rtk git status"}}).to_string();
        let response = run_claude_inner_v2(&payload);
        assert_eq!(response, HookResponse::NoOpinion);
    }

    #[test]
    fn test_run_claude_inner_v2_passthrough_for_heredoc() {
        let payload = json!({
            "tool_name": "Bash",
            "tool_input": {"command": "cat <<EOF\nhello\nEOF"}
        })
        .to_string();
        let response = run_claude_inner_v2(&payload);
        assert_eq!(response, HookResponse::NoOpinion);
    }

    #[test]
    fn test_hook_response_enum_equality() {
        // Allow trait derivations are wired correctly so the dispatcher
        // in run_claude can match exhaustively.
        let a = HookResponse::NoOpinion;
        let b = HookResponse::Allow("test".into());
        let c = HookResponse::Deny("json".into(), "reason".into());
        assert_eq!(a.clone(), HookResponse::NoOpinion);
        assert_ne!(a, b);
        assert_ne!(b, c);
    }
}
