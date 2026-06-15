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
    /// GitHub Copilot CLI: camelCase `toolName` + `toolArgs` (JSON string), supports `modifiedArgs` for transparent rewrite.
    /// Carries the full parsed `toolArgs` object so we can rewrite `command` while preserving
    /// host-supplied metadata (description, initial_wait, mode, …) the tool requires.
    CopilotCli { command: String, args: Value },
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

    // Strip leading BOM(s) before trimming: some Windows hosts prepend UTF-8
    // BOMs to hook stdin (confirmed for Cursor), which serde_json rejects.
    let input = strip_leading_bom(&input).trim();
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
        HookFormat::CopilotCli { command, args } => handle_copilot_cli(&command, &args),
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
                            args: tool_args,
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

enum HookDecision {
    AllowRewrite(String),
    AskRewrite(String),
    Defer,
    Deny,
}

fn decide_from_verdict(cmd: &str, verdict: PermissionVerdict) -> HookDecision {
    if verdict == PermissionVerdict::Deny {
        return HookDecision::Deny;
    }
    if crate::discover::lexer::contains_unattestable_construct(cmd) {
        return HookDecision::Defer;
    }
    match get_rewritten(cmd) {
        Some(r) if verdict == PermissionVerdict::Allow => HookDecision::AllowRewrite(r),
        Some(r) => HookDecision::AskRewrite(r),
        None => HookDecision::Defer,
    }
}

fn decide_hook_action(cmd: &str, host: permissions::Host) -> HookDecision {
    decide_from_verdict(cmd, permissions::check_command_for(cmd, host))
}

fn handle_vscode(cmd: &str) -> Result<()> {
    let (decision, rewritten) = match decide_hook_action(cmd, permissions::Host::Claude) {
        HookDecision::Deny => {
            audit_log("deny", cmd, "");
            return Ok(());
        }
        HookDecision::Defer => return Ok(()),
        HookDecision::AllowRewrite(r) => ("allow", r),
        HookDecision::AskRewrite(r) => ("ask", r),
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

fn handle_copilot_cli(cmd: &str, args: &Value) -> Result<()> {
    if let Some(response) = copilot_cli_response(cmd, args) {
        let _ = writeln!(io::stdout(), "{response}");
    }
    Ok(())
}

fn copilot_cli_response(cmd: &str, args: &Value) -> Option<Value> {
    copilot_cli_response_from_decision(
        args,
        decide_hook_action(cmd, permissions::Host::Claude),
        cmd,
    )
}

fn copilot_cli_response_from_decision(
    args: &Value,
    decision: HookDecision,
    cmd: &str,
) -> Option<Value> {
    let (rewritten, allow) = match decision {
        HookDecision::Deny => {
            audit_log("deny", cmd, "");
            return None;
        }
        HookDecision::Defer => return None,
        HookDecision::AllowRewrite(r) => (r, true),
        HookDecision::AskRewrite(r) => (r, false),
    };

    audit_log("rewrite", cmd, &rewritten);

    let mut modified = args.clone();
    if let Some(obj) = modified.as_object_mut() {
        obj.insert("command".into(), Value::String(rewritten));
    }

    let mut response = json!({
        "permissionDecisionReason": "RTK auto-rewrite",
        "modifiedArgs": modified,
    });
    if allow {
        response["permissionDecision"] = json!("allow");
    }
    Some(response)
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

    match decide_hook_action(cmd, permissions::Host::Gemini) {
        HookDecision::Deny => {
            let _ = writeln!(
                io::stdout(),
                r#"{{"decision":"deny","reason":"Blocked by RTK permission rule"}}"#
            );
        }
        HookDecision::AllowRewrite(ref rewritten) => {
            audit_log("rewrite", cmd, rewritten);
            print_gemini("allow", Some(rewritten));
        }
        HookDecision::AskRewrite(ref rewritten) => {
            audit_log("ask", cmd, rewritten);
            print_gemini("ask_user", Some(rewritten));
        }
        HookDecision::Defer => print_gemini("ask_user", None),
    }

    Ok(())
}

fn print_allow() {
    let _ = writeln!(io::stdout(), r#"{{"decision":"allow"}}"#);
}

fn gemini_json(decision: &str, rewrite: Option<&str>) -> String {
    let mut output = serde_json::json!({ "decision": decision });
    if let Some(cmd) = rewrite {
        output["hookSpecificOutput"] = serde_json::json!({ "tool_input": { "command": cmd } });
    }
    output.to_string()
}

fn print_gemini(decision: &str, rewrite: Option<&str>) {
    let _ = writeln!(io::stdout(), "{}", gemini_json(decision, rewrite));
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

    let (rewritten, allow) = match decide_hook_action(cmd, permissions::Host::Claude) {
        HookDecision::Deny => {
            return PayloadAction::Skip {
                reason: "skip:deny_rule",
                cmd: cmd.to_string(),
            }
        }
        HookDecision::Defer => {
            return PayloadAction::Skip {
                reason: "skip:defer",
                cmd: cmd.to_string(),
            }
        }
        HookDecision::AllowRewrite(r) => (r, true),
        HookDecision::AskRewrite(r) => (r, false),
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

    if allow {
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
///
/// Issue #1773: every NoOpinion exit emits `{}` (valid JSON, "no opinion")
/// instead of zero bytes. Claude Code's hook runner treats an empty stream
/// as a malformed response, so the placeholder JSON keeps downstream JSON
/// consumers happy.
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
        // Issue #1773: emit valid JSON so the hook runner does not see
        // zero bytes when input is empty.
        let _ = writeln!(io::stdout(), "{{}}");
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
                    // Issue #1773: NoOpinion + no manifest block → emit `{}`
                    // (valid "no opinion" JSON) instead of zero bytes.
                    let _ = writeln!(io::stdout(), "{{}}");
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

/// Issue #1773: Returns the exact JSON string that `run_claude` would write
/// to stdout for the given raw input. Mirrors the branching in `run_claude`
/// without performing real I/O so tests can assert on the stdout contract.
#[cfg(test)]
fn run_claude_stdout(input: &str) -> String {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return "{}\n".to_string();
    }
    let v: Value = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(_) => return "{}\n".to_string(),
    };
    match process_claude_payload(&v) {
        PayloadAction::Rewrite { output, .. } => format!("{output}\n"),
        PayloadAction::Skip { .. } | PayloadAction::Ignore => "{}\n".to_string(),
    }
}

// ── Codex CLI native hook ──────────────────────────────────────
//
// Codex's PreToolUse protocol (per developers.openai.com/codex/hooks):
//   stdin  : {"hook_event_name":"PreToolUse","tool_name":"Bash",
//             "tool_input":{"command":"..."}, session_id, turn_id, cwd, ...}
//   stdout : {"hookSpecificOutput":{"hookEventName":"PreToolUse",
//             "permissionDecision":"allow|deny", "updatedInput":{"command":"..."}}}
//
// The response schema is byte-identical to Claude's, so we reuse
// `process_claude_payload`. Codex-specific differences live entirely in
// `run_codex`: it emits `{}` (Codex's neutral no-opinion) on every
// non-rewrite path (empty stdin, malformed JSON, non-Bash tool, skip,
// ignore) rather than going silent. Codex semantics permit multiple
// matching hooks to run concurrently — unlike Claude #1515, there is
// no manifest fallthrough to engineer here.

/// Run the Codex CLI PreToolUse hook natively.
pub fn run_codex() -> Result<()> {
    let input = read_stdin_limited()?;
    let input = input.trim();
    if input.is_empty() {
        let _ = writeln!(io::stdout(), "{{}}");
        return Ok(());
    }

    let v: Value = match serde_json::from_str(input) {
        Ok(v) => v,
        Err(_) => {
            // Fail-open: emit neutral no-opinion, no stderr noise.
            let _ = writeln!(io::stdout(), "{{}}");
            return Ok(());
        }
    };

    // Codex currently fires PreToolUse only for the Bash tool; defend
    // against future event/tool expansion by emitting `{}` for anything
    // else rather than misinterpreting the payload.
    let tool_name = v.get("tool_name").and_then(|t| t.as_str()).unwrap_or("");
    if tool_name != "Bash" {
        let _ = writeln!(io::stdout(), "{{}}");
        return Ok(());
    }

    match process_claude_payload(&v) {
        PayloadAction::Rewrite {
            cmd,
            rewritten,
            output,
        } => {
            audit_log("rewrite", &cmd, &rewritten);
            let _ = writeln!(io::stdout(), "{output}");
        }
        PayloadAction::Skip { reason, cmd } => {
            audit_log(reason, &cmd, "");
            let _ = writeln!(io::stdout(), "{{}}");
        }
        PayloadAction::Ignore => {
            let _ = writeln!(io::stdout(), "{{}}");
        }
    }

    Ok(())
}

#[cfg(test)]
fn run_codex_inner(input: &str) -> Option<String> {
    let v: Value = serde_json::from_str(input).ok()?;
    let tool_name = v.get("tool_name").and_then(|t| t.as_str()).unwrap_or("");
    if tool_name != "Bash" {
        return None;
    }
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

    let output = match decide_hook_action(&cmd, permissions::Host::Cursor) {
        HookDecision::AllowRewrite(rewritten) => {
            audit_log("rewrite", &cmd, &rewritten);
            cursor_allow(&rewritten)
        }
        other => {
            if matches!(other, HookDecision::Deny) {
                audit_log("deny", &cmd, "");
            }
            "{}".to_string()
        }
    };
    let _ = writeln!(io::stdout(), "{output}");
    Ok(())
}

fn cursor_allow(rewritten: &str) -> String {
    json!({
        "continue": true,
        "permission": "allow",
        "updated_input": { "command": rewritten }
    })
    .to_string()
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
    match decide_from_verdict(&cmd, verdict) {
        HookDecision::AllowRewrite(rewritten) => cursor_allow(&rewritten),
        _ => "{}".to_string(),
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
    fn test_copilot_bom_prefixed_payload_is_recognized() {
        // Windows hosts may prepend one or two UTF-8 BOMs to hook stdin
        // (confirmed for Cursor). run_copilot strips them before parsing;
        // verify both Copilot formats still parse after the same handling.
        for raw in [
            format!("\u{feff}{}", copilot_cli_input("git status")),
            format!("\u{feff}\u{feff}{}", copilot_cli_input("git status")),
        ] {
            let cleaned = strip_leading_bom(&raw).trim();
            let v: Value = serde_json::from_str(cleaned).expect("BOM-stripped JSON must parse");
            assert!(matches!(detect_format(&v), HookFormat::CopilotCli { .. }));
        }

        let raw = format!("\u{feff}{}", vscode_input("Bash", "git status"));
        let v: Value = serde_json::from_str(strip_leading_bom(&raw).trim()).unwrap();
        assert!(matches!(detect_format(&v), HookFormat::VsCode { .. }));
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

    // --- Copilot CLI handler: transparent rewrite via modifiedArgs ---

    fn cli_args(cmd: &str) -> Value {
        json!({ "command": cmd })
    }

    #[test]
    fn test_copilot_cli_ask_rewrite_omits_permission_decision() {
        let r = copilot_cli_response_from_decision(
            &cli_args("cargo test"),
            HookDecision::AskRewrite("rtk cargo test".into()),
            "cargo test",
        )
        .unwrap();
        assert!(
            r.get("permissionDecision").is_none(),
            "AskRewrite must NOT set permissionDecision — Copilot then runs its normal prompt flow on the rewritten command"
        );
        assert_eq!(r["modifiedArgs"]["command"], "rtk cargo test");
    }

    #[test]
    fn test_copilot_cli_allow_rewrite_returns_allow() {
        let r = copilot_cli_response_from_decision(
            &cli_args("cargo test"),
            HookDecision::AllowRewrite("rtk cargo test".into()),
            "cargo test",
        )
        .unwrap();
        assert_eq!(r["permissionDecision"], "allow");
        assert_eq!(r["modifiedArgs"]["command"], "rtk cargo test");
    }

    #[test]
    fn test_copilot_cli_deny_returns_none() {
        assert!(copilot_cli_response_from_decision(
            &cli_args("cargo test"),
            HookDecision::Deny,
            "cargo test",
        )
        .is_none());
    }

    #[test]
    fn test_copilot_cli_defer_returns_none() {
        // Defer covers both "no rewrite available" and the unattestable-construct gate.
        // The hook must emit NO modifiedArgs for CVE bypass forms — no laundering.
        assert!(copilot_cli_response_from_decision(
            &cli_args("git status & rm -rf /tmp/x"),
            HookDecision::Defer,
            "git status & rm -rf /tmp/x",
        )
        .is_none());
    }

    #[test]
    fn test_copilot_cli_passthrough_unsupported() {
        assert!(copilot_cli_response("htop", &cli_args("htop")).is_none());
    }

    #[test]
    fn test_copilot_cli_passthrough_already_rtk() {
        assert!(copilot_cli_response("rtk cargo test", &cli_args("rtk cargo test")).is_none());
    }

    #[test]
    fn test_copilot_cli_passthrough_heredoc() {
        let cmd = "cat <<EOF\nhi\nEOF";
        assert!(copilot_cli_response(cmd, &cli_args(cmd)).is_none());
    }

    #[test]
    fn test_copilot_cli_preserves_env_prefix() {
        let r = copilot_cli_response(
            "RUST_LOG=debug cargo test",
            &cli_args("RUST_LOG=debug cargo test"),
        )
        .unwrap();
        assert_eq!(
            r["modifiedArgs"]["command"],
            "RUST_LOG=debug rtk cargo test"
        );
    }

    #[test]
    fn test_copilot_cli_preserves_extra_args_fields() {
        let args = json!({
            "command": "cargo install ripgrep",
            "description": "install ripgrep",
            "initial_wait": 30,
            "mode": "sync"
        });
        let r = copilot_cli_response_from_decision(
            &args,
            HookDecision::AskRewrite("rtk cargo install ripgrep".into()),
            "cargo install ripgrep",
        )
        .unwrap();
        let modified = &r["modifiedArgs"];
        assert_eq!(modified["command"], "rtk cargo install ripgrep");
        assert_eq!(modified["description"], "install ripgrep");
        assert_eq!(modified["initial_wait"], 30);
        assert_eq!(modified["mode"], "sync");
    }

    fn end_to_end(cmd: &str) -> Option<Value> {
        let verdict = crate::hooks::permissions::check_command_with_rules(
            cmd,
            &[],
            &[],
            &["Bash(git:*)".to_string()],
        );
        copilot_cli_response_from_decision(&cli_args(cmd), decide_from_verdict(cmd, verdict), cmd)
    }

    #[test]
    fn test_copilot_cli_cve_safe_forms_still_rewrite() {
        for cmd in ["git status", "git status 2>&1"] {
            let r = end_to_end(cmd).unwrap_or_else(|| panic!("expected rewrite for {cmd:?}"));
            assert_eq!(
                r["modifiedArgs"]["command"].as_str().unwrap(),
                format!("rtk {cmd}"),
                "safe form {cmd:?} must rewrite",
            );
        }
    }

    #[test]
    fn test_copilot_cli_cve_newline_bypass_never_auto_allows() {
        let r = end_to_end("git status\nrm -rf /tmp/x");
        if let Some(resp) = r {
            assert!(
                resp.get("permissionDecision").is_none(),
                "newline-hidden command must not produce permissionDecision: \"allow\""
            );
        }
    }

    #[test]
    fn test_copilot_cli_cve_background_bypass_never_auto_allows() {
        let r = end_to_end("git status & rm -rf /tmp/x");
        if let Some(resp) = r {
            assert!(
                resp.get("permissionDecision").is_none(),
                "background-& hidden command must not produce permissionDecision: \"allow\""
            );
        }
    }

    #[test]
    fn test_copilot_cli_cve_command_substitution_returns_none() {
        assert!(
            end_to_end("git log --pretty=$(rm -rf /tmp/x)").is_none(),
            "$( ) command substitution must not produce modifiedArgs"
        );
    }

    #[test]
    fn test_copilot_cli_cve_backtick_substitution_returns_none() {
        assert!(
            end_to_end("git log --pretty=`rm -rf /tmp/x`").is_none(),
            "backtick substitution must not produce modifiedArgs"
        );
    }

    #[test]
    fn test_copilot_cli_cve_file_redirect_amp_returns_none() {
        assert!(
            end_to_end("git status >& /tmp/evil").is_none(),
            ">&file redirect must not produce modifiedArgs"
        );
    }

    #[test]
    fn test_copilot_cli_cve_file_redirect_returns_none() {
        assert!(
            end_to_end("git status > /tmp/evil").is_none(),
            ">file redirect must not produce modifiedArgs"
        );
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
    fn test_claude_substitution_not_rewritten() {
        // A substitution payload must never be rewritten into updatedInput;
        // RTK skips so Claude Code evaluates the original command natively.
        assert!(run_claude_inner(&claude_input("git status `rm -rf /tmp/x`")).is_none());
        assert!(run_claude_inner(&claude_input("git status $(rm -rf /tmp/x)")).is_none());
        assert!(run_claude_inner(&claude_input("git log --pretty=\"$(rm -rf /tmp/x)\"")).is_none());
    }

    #[test]
    fn test_claude_file_redirect_not_rewritten() {
        assert!(run_claude_inner(&claude_input("git log > /tmp/out.txt")).is_none());
    }

    #[test]
    fn test_claude_fd_dup_redirect_still_rewritten() {
        // `2>&1` is attestable — the rewrite proceeds as normal.
        assert!(run_claude_inner(&claude_input("git status 2>&1")).is_some());
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

    // --- Issue #1773: never emit empty stdout from `rtk hook claude` ---

    /// Helper: assert that `run_claude_stdout` produces parseable JSON, never
    /// the empty string. This is the contract Claude Code's hook system relies
    /// on — zero bytes was misinterpreted as a malformed response.
    fn assert_emits_valid_json(input: &str, context: &str) {
        let out = run_claude_stdout(input);
        assert!(
            !out.is_empty(),
            "{}: empty stdout violates Claude Code hook contract",
            context
        );
        // Trailing newline is part of writeln!; strip before parsing.
        let trimmed = out.trim_end_matches('\n');
        let _: Value = serde_json::from_str(trimmed)
            .unwrap_or_else(|e| panic!("{}: stdout is not valid JSON: {:?} ({})", context, out, e));
    }

    #[test]
    fn test_claude_no_rewrite_emits_empty_object() {
        // Commands RTK cannot rewrite must still emit valid JSON.
        let out = run_claude_stdout(&claude_input("head -c 100 /etc/hosts"));
        assert_eq!(out, "{}\n");
    }

    #[test]
    fn test_claude_unknown_command_emits_empty_object() {
        let out = run_claude_stdout(&claude_input("htop"));
        assert_eq!(out, "{}\n");
    }

    #[test]
    fn test_claude_empty_command_emits_empty_object() {
        let input = json!({
            "tool_name": "Bash",
            "tool_input": { "command": "" }
        })
        .to_string();
        assert_eq!(run_claude_stdout(&input), "{}\n");
    }

    #[test]
    fn test_claude_missing_tool_input_emits_empty_object() {
        let input = json!({ "tool_name": "Bash" }).to_string();
        assert_eq!(run_claude_stdout(&input), "{}\n");
    }

    #[test]
    fn test_claude_empty_input_emits_empty_object() {
        assert_eq!(run_claude_stdout(""), "{}\n");
        assert_eq!(run_claude_stdout("   "), "{}\n");
        assert_eq!(run_claude_stdout("\n\n"), "{}\n");
    }

    #[test]
    fn test_claude_malformed_json_emits_empty_object() {
        assert_eq!(run_claude_stdout("not valid json {{{"), "{}\n");
    }

    #[test]
    fn test_claude_rewrite_still_emits_full_response() {
        // Regression: rewritable commands must still emit the full hookSpecificOutput.
        let out = run_claude_stdout(&claude_input("git status"));
        assert!(out.contains("hookSpecificOutput"));
        assert!(out.contains("rtk git status"));
    }

    #[test]
    fn test_claude_no_rewrite_cases_all_emit_valid_json() {
        // Issue #1773 lists specific commands. Verify each emits JSON.
        for cmd in &[
            "head -c 100 file.txt",
            "wc -l file.txt",
            "sed 's/a/b/' file.txt",
            "yarn test",
            "npm run build",
            "htop",
            "ls -la", // ls IS rewritten, but verify valid JSON regardless
        ] {
            assert_emits_valid_json(&claude_input(cmd), &format!("command: {cmd:?}"));
        }
    }

    // --- Codex handler ---

    fn codex_input(cmd: &str) -> String {
        // Codex PreToolUse stdin includes session/turn metadata that RTK
        // ignores, plus the canonical Bash payload. Tests use the full
        // shape to catch regressions where extra fields confuse parsing.
        json!({
            "session_id": "s-1234",
            "tool_use_id": "tu-5678",
            "turn_id": "t-9012",
            "cwd": "/repo",
            "permission_mode": "default",
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": { "command": cmd }
        })
        .to_string()
    }

    #[test]
    fn test_codex_rewrite_git_status() {
        let result = run_codex_inner(&codex_input("git status")).unwrap();
        let v: Value = serde_json::from_str(&result).unwrap();
        let cmd = v
            .pointer("/hookSpecificOutput/updatedInput/command")
            .and_then(|c| c.as_str())
            .unwrap();
        assert_eq!(cmd, "rtk git status");
    }

    #[test]
    fn test_codex_output_matches_claude_schema_byte_for_byte() {
        // RTK reuses process_claude_payload because the OpenAI Codex
        // PreToolUse response schema is identical to Claude's. This test
        // pins that invariant so a future Claude-only tweak that drifts
        // the schema gets caught.
        let codex = run_codex_inner(&codex_input("git status")).unwrap();
        let claude = run_claude_inner(&claude_input("git status")).unwrap();
        assert_eq!(codex, claude);
    }

    #[test]
    fn test_codex_hookspecificoutput_structure() {
        let result = run_codex_inner(&codex_input("git status")).unwrap();
        let v: Value = serde_json::from_str(&result).unwrap();
        let hook = &v["hookSpecificOutput"];
        assert_eq!(hook["hookEventName"], PRE_TOOL_USE_KEY);
        assert_eq!(hook["permissionDecisionReason"], "RTK auto-rewrite");
        assert!(hook["updatedInput"].is_object());
        assert!(hook["updatedInput"]["command"].is_string());
    }

    #[test]
    fn test_codex_non_bash_tool_returns_none() {
        // Codex PreToolUse currently only fires for Bash, but defensive:
        // if a future Codex sends shell_view or fs_read here, RTK must
        // refuse rather than misinterpret.
        let input = json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "shell_view",
            "tool_input": { "command": "git status" }
        })
        .to_string();
        assert!(run_codex_inner(&input).is_none());
    }

    #[test]
    fn test_codex_passthrough_no_output() {
        // htop has no RTK rewrite — payload returns None (-> "{}" in I/O wrapper).
        assert!(run_codex_inner(&codex_input("htop")).is_none());
    }

    #[test]
    fn test_codex_already_rtk_passthrough() {
        assert!(run_codex_inner(&codex_input("rtk git status")).is_none());
    }

    #[test]
    fn test_codex_compound_command_rewrites_both_segments() {
        let result = run_codex_inner(&codex_input("git add . && cargo test")).unwrap();
        let v: Value = serde_json::from_str(&result).unwrap();
        let cmd = v
            .pointer("/hookSpecificOutput/updatedInput/command")
            .and_then(|c| c.as_str())
            .unwrap();
        assert_eq!(cmd, "rtk git add . && rtk cargo test");
    }

    #[test]
    fn test_codex_env_prefix_preserved() {
        let result = run_codex_inner(&codex_input("GIT_PAGER=cat git status")).unwrap();
        let v: Value = serde_json::from_str(&result).unwrap();
        let cmd = v
            .pointer("/hookSpecificOutput/updatedInput/command")
            .and_then(|c| c.as_str())
            .unwrap();
        assert_eq!(cmd, "GIT_PAGER=cat rtk git status");
    }

    #[test]
    fn test_codex_substitution_not_rewritten() {
        // CVE-class: command substitution payloads must skip so Codex
        // applies its own permission policy instead of auto-allowing.
        assert!(run_codex_inner(&codex_input("git status `rm -rf /tmp/x`")).is_none());
        assert!(run_codex_inner(&codex_input("git status $(rm -rf /tmp/x)")).is_none());
    }

    #[test]
    fn test_codex_file_redirect_not_rewritten() {
        assert!(run_codex_inner(&codex_input("git log > /tmp/out.txt")).is_none());
    }

    #[test]
    fn test_codex_empty_command_returns_none() {
        let input = json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": { "command": "" }
        })
        .to_string();
        assert!(run_codex_inner(&input).is_none());
    }

    #[test]
    fn test_codex_malformed_json_returns_none() {
        assert!(run_codex_inner("not valid json {{{").is_none());
    }

    #[test]
    fn test_codex_no_tool_input_returns_none() {
        let input = json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash"
        })
        .to_string();
        assert!(run_codex_inner(&input).is_none());
    }

    #[test]
    fn test_codex_extra_session_fields_ignored() {
        // session_id / turn_id / cwd / permission_mode must not affect
        // the rewrite. codex_input() already adds them — this verifies
        // a smaller payload with NONE of them rewrites identically.
        let minimal = json!({
            "tool_name": "Bash",
            "tool_input": { "command": "git status" }
        })
        .to_string();
        let full = codex_input("git status");
        let minimal_out = run_codex_inner(&minimal).unwrap();
        let full_out = run_codex_inner(&full).unwrap();
        assert_eq!(minimal_out, full_out);
    }

    // --- Cursor handler ---

    fn cursor_input(cmd: &str) -> String {
        json!({
            "tool_name": "Bash",
            "tool_input": { "command": cmd }
        })
        .to_string()
    }

    fn run_cursor_allowed(input: &str) -> String {
        run_cursor_inner_with_rules(input, &[], &[], &["*".to_string()])
    }

    #[test]
    fn test_cursor_rewrite_flat_format() {
        let result = run_cursor_allowed(&cursor_input("git status"));
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
    fn test_cursor_no_allow_rule_defers() {
        assert_eq!(run_cursor_inner(&cursor_input("git status")), "{}");
    }

    #[test]
    fn test_cursor_substitution_defers_even_when_allowed() {
        assert_eq!(
            run_cursor_allowed(&cursor_input("git status `rm -rf /tmp/x`")),
            "{}"
        );
        assert_eq!(
            run_cursor_allowed(&cursor_input("git status $(rm -rf /tmp/x)")),
            "{}"
        );
    }

    #[test]
    fn test_cursor_unallowed_segment_defers() {
        let out = run_cursor_inner_with_rules(
            &cursor_input("git status && rm -rf /tmp/x"),
            &[],
            &[],
            &["git *".to_string()],
        );
        assert_eq!(out, "{}");
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
        let result = run_cursor_allowed(&cursor_input("cargo test"));
        let v: Value = serde_json::from_str(&result).unwrap();
        assert!(v.get("hookSpecificOutput").is_none());
        assert_eq!(v["permission"], "allow");
        assert_eq!(v["continue"], true);
    }

    #[test]
    fn test_cursor_compound_rewrite_includes_continue() {
        let cmd = "cd \"/tmp/proj\" && git status";
        let result = run_cursor_allowed(&cursor_input(cmd));
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
        let result = run_cursor_allowed(&with_single_bom);
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
        let result = run_cursor_allowed(&with_double_bom);
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

    // --- Shared decision flow (all hosts route through this) ---

    fn decide_with_rules(
        cmd: &str,
        deny: &[String],
        ask: &[String],
        allow: &[String],
    ) -> HookDecision {
        let verdict = permissions::check_command_with_rules(cmd, deny, ask, allow);
        decide_from_verdict(cmd, verdict)
    }

    fn all_allowed() -> Vec<String> {
        vec!["*".to_string()]
    }

    #[test]
    fn test_decide_allow_for_attestable_allowed_command() {
        assert!(matches!(
            decide_with_rules("git status", &[], &[], &all_allowed()),
            HookDecision::AllowRewrite(_)
        ));
    }

    #[test]
    fn test_decide_ask_for_default_verdict() {
        assert!(matches!(
            decide_with_rules("git status", &[], &[], &[]),
            HookDecision::AskRewrite(_)
        ));
    }

    #[test]
    fn test_decide_deny() {
        assert!(matches!(
            decide_with_rules(
                "rm -rf /tmp/x",
                &["rm -rf".to_string()],
                &[],
                &all_allowed()
            ),
            HookDecision::Deny
        ));
    }

    #[test]
    fn test_decide_defer_for_substitution_even_when_allowed() {
        for cmd in [
            "git status `rm -rf /tmp/x`",
            "git status $(rm -rf /tmp/x)",
            "git log --pretty=\"$(rm -rf /tmp/x)\"",
        ] {
            assert!(
                matches!(
                    decide_with_rules(cmd, &[], &[], &all_allowed()),
                    HookDecision::Defer
                ),
                "expected Defer for {cmd}"
            );
        }
    }

    #[test]
    fn test_decide_defer_for_file_redirect() {
        assert!(matches!(
            decide_with_rules("git log > /tmp/out.txt", &[], &[], &all_allowed()),
            HookDecision::Defer
        ));
    }

    #[test]
    fn test_decide_allow_for_fd_dup_redirect() {
        assert!(matches!(
            decide_with_rules("git status 2>&1", &[], &[], &all_allowed()),
            HookDecision::AllowRewrite(_)
        ));
    }

    // --- Gemini rendering ---

    fn gemini_render(cmd: &str, deny: &[String], ask: &[String], allow: &[String]) -> String {
        match decide_with_rules(cmd, deny, ask, allow) {
            HookDecision::Deny => {
                r#"{"decision":"deny","reason":"Blocked by RTK permission rule"}"#.to_string()
            }
            HookDecision::AllowRewrite(r) => gemini_json("allow", Some(&r)),
            HookDecision::AskRewrite(r) => gemini_json("ask_user", Some(&r)),
            HookDecision::Defer => gemini_json("ask_user", None),
        }
    }

    #[test]
    fn test_gemini_allow_emits_rewrite() {
        let v: Value =
            serde_json::from_str(&gemini_render("git status", &[], &[], &all_allowed())).unwrap();
        assert_eq!(v["decision"], "allow");
        assert_eq!(
            v["hookSpecificOutput"]["tool_input"]["command"],
            "rtk git status"
        );
    }

    #[test]
    fn test_gemini_default_asks_user() {
        let v: Value = serde_json::from_str(&gemini_render("git status", &[], &[], &[])).unwrap();
        assert_eq!(v["decision"], "ask_user");
    }

    #[test]
    fn test_gemini_substitution_asks_user_without_rewrite() {
        let v: Value = serde_json::from_str(&gemini_render(
            "git status `rm -rf /tmp/x`",
            &[],
            &[],
            &all_allowed(),
        ))
        .unwrap();
        assert_eq!(v["decision"], "ask_user");
        assert!(v.get("hookSpecificOutput").is_none());
    }

    #[test]
    fn test_gemini_deny_decision() {
        let v: Value = serde_json::from_str(&gemini_render(
            "rm -rf /tmp/x",
            &["rm -rf".to_string()],
            &[],
            &[],
        ))
        .unwrap();
        assert_eq!(v["decision"], "deny");
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
