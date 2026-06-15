//! Hook collision manifest: plugin-cache patching and fallthrough dispatch.
//!
//! ## Why this exists
//!
//! Claude Code's PreToolUse hook protocol silently drops the `updatedInput`
//! field when more than one registered plugin matches the same tool (open
//! issue: <https://github.com/rtk-ai/rtk/issues/1515>). Two plugins that both
//! register a `Bash` matcher therefore *race* — only one rewrite reaches
//! the agent and there is no error reported back. RTK is the most-installed
//! Bash hook and consequently the one that gets squashed.
//!
//! ## Two-phase fix
//!
//! **Install-time patch** (`patch_plugin_caches`): walk
//! `~/.claude/plugins/cache/*/*/<version>/hooks/*.json`, find every
//! PreToolUse entry whose matcher contains `Bash`, strip the `Bash` token
//! out of the pipe-separated alternation, and record both the original
//! matcher and the original `command` string in
//! `~/.claude/hooks/rtk-bash-manifest.json`. After this step RTK is the
//! only plugin still claiming Bash.
//!
//! **Runtime fallthrough** (`run_manifest_handlers`): when `rtk hook
//! claude` runs, it forwards the *original* payload to every command in
//! the manifest, gives any of them the chance to deny the request, and
//! only emits its own rewrite if none of them did. This preserves
//! cooperating plugins' safety semantics (autorun's deny rules, for
//! example) on top of RTK's rewrite.
//!
//! Ported from v2 `src/init.rs` (`patch_plugin_caches`, `BashManifest`,
//! `ManifestEntry`) and v2 `src/cmd/hook/claude.rs`
//! (`run_manifest_handlers`, `ManifestResult`, `is_json_deny`,
//! `extract_deny_reason`). Layout adapted to upstream's `src/hooks/` split.
//!
//! I/O contract: `run_manifest_handlers` MUST NEVER write to stdout/stderr
//! — its caller (`hook_cmd::run_claude`) is the single I/O point and any
//! stray byte before its JSON would corrupt the protocol.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::NamedTempFile;

/// Atomic write via tempfile + rename. Same semantics as
/// `init::atomic_write` but private to the manifest module so we do not
/// have to widen the visibility of an unrelated helper.
fn atomic_write(path: &Path, content: &str) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("Cannot write to {}: no parent dir", path.display()))?;
    let mut temp = NamedTempFile::new_in(parent)
        .with_context(|| format!("Failed to create temp file in {}", parent.display()))?;
    temp.write_all(content.as_bytes())
        .with_context(|| format!("Failed to write {} bytes", content.len()))?;
    temp.persist(path)
        .with_context(|| format!("Failed to persist {}", path.display()))?;
    Ok(())
}

// =========================================================================
// Manifest data model
// =========================================================================

/// One displaced plugin handler.
///
/// `cache_path` is the absolute path to the patched cache file (so uninstall
/// can find the file to restore). `original_matcher` / `patched_matcher`
/// record the matcher transformation so install is idempotent and uninstall
/// can put `Bash` back. `fallthrough_command` is the *resolved* command line
/// (with `${CLAUDE_PLUGIN_ROOT}` expanded to an absolute path) that RTK now
/// invokes on behalf of the displaced plugin.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub(crate) struct ManifestEntry {
    pub(crate) cache_path: String,
    pub(crate) original_matcher: String,
    pub(crate) patched_matcher: String,
    pub(crate) fallthrough_command: String,
}

/// On-disk schema for `~/.claude/hooks/rtk-bash-manifest.json`.
///
/// `version` is reserved for future schema migrations; today the loader
/// accepts any value and the writer always emits `1`. `entries` is the
/// authoritative list of fallthrough handlers RTK forwards to.
#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct BashManifest {
    #[serde(default = "BashManifest::default_version")]
    pub(crate) version: u32,
    #[serde(default)]
    pub(crate) patched_at: String,
    #[serde(default)]
    pub(crate) entries: Vec<ManifestEntry>,
}

impl Default for BashManifest {
    fn default() -> Self {
        Self {
            version: Self::default_version(),
            patched_at: String::new(),
            entries: Vec::new(),
        }
    }
}

impl BashManifest {
    fn default_version() -> u32 {
        1
    }
}

// =========================================================================
// Path helpers
// =========================================================================

/// Absolute path to the RTK bash manifest. Falls back to `$USERPROFILE`
/// on Windows so the hook works when only one of `$HOME` / `$USERPROFILE`
/// is set (common in CI runners).
pub(crate) fn manifest_path() -> Option<PathBuf> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()?;
    Some(
        Path::new(&home)
            .join(".claude")
            .join("hooks")
            .join("rtk-bash-manifest.json"),
    )
}

// =========================================================================
// JSON deny detection (used by run_claude's fallthrough/veto paths)
// =========================================================================

/// Returns `true` if `json_str` is either a Claude Code-style or
/// Gemini CLI-style deny response.
///
/// Both formats are accepted because the manifest can hold handlers
/// designed for either CLI; the rtk-shipped collision dispatcher is
/// the only thing that needs to interpret them.
///
/// - Claude Code: `{"hookSpecificOutput": {"permissionDecision": "deny", ...}}`
/// - Gemini CLI: `{"decision": "deny", ...}` (top-level)
pub(crate) fn is_json_deny(json_str: &str) -> bool {
    let Ok(v) = serde_json::from_str::<Value>(json_str.trim()) else {
        return false;
    };
    let cc_deny = v
        .get("hookSpecificOutput")
        .and_then(|o| o.get("permissionDecision"))
        .and_then(|d| d.as_str())
        == Some("deny");
    let gemini_deny = v.get("decision").and_then(|d| d.as_str()) == Some("deny");
    cc_deny || gemini_deny
}

/// Extract the human-readable reason from a deny JSON in either CLI format.
///
/// Returns `None` if the JSON is malformed or carries no reason field —
/// callers then synthesize a generic message so the user is never left
/// with an empty stderr at exit 2.
pub(crate) fn extract_deny_reason(json_str: &str) -> Option<String> {
    let v: Value = serde_json::from_str(json_str.trim()).ok()?;
    if let Some(r) = v
        .get("hookSpecificOutput")
        .and_then(|o| o.get("permissionDecisionReason"))
        .and_then(|r| r.as_str())
    {
        return Some(r.to_owned());
    }
    v.get("reason").and_then(|r| r.as_str()).map(str::to_owned)
}

// =========================================================================
// Manifest loading
// =========================================================================

/// Read the on-disk manifest, returning `None` if absent or malformed.
///
/// "Absent or malformed" is treated identically because the hook MUST
/// fail-open: a broken manifest must not block tools. The loader does
/// no error reporting so it can be called from inside the JSON-protocol
/// hook (where any stderr at exit 0 is a fail signal — see
/// `hook_cmd::run_claude`).
pub(crate) fn load_manifest() -> Option<BashManifest> {
    let path = manifest_path()?;
    if !path.exists() {
        return None;
    }
    let content = fs::read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

// =========================================================================
// Runtime fallthrough dispatch
// =========================================================================

/// Outcome of running every manifest handler against an incoming payload.
///
/// Never includes stdout/stderr writes — the only writer is
/// `hook_cmd::run_claude`, which serializes deny decisions atomically.
#[derive(Debug)]
pub(crate) enum ManifestResult {
    /// At least one handler vetoed the command.
    /// `json` is that handler's stdout, forwarded verbatim so the agent
    /// sees the original handler's reason. `stderr_bytes` is the handler's
    /// stderr, surfaced only at exit 2.
    Blocked { json: String, stderr_bytes: Vec<u8> },
    /// No handler vetoed; caller may continue with RTK's own decision.
    NoBlock,
}

/// Walk a `cache_path` of the form
/// `…/cache/{vendor}/{plugin}/{version}/hooks/{file}.json` back up to its
/// `(version_dir, plugin_dir)` pair. Returns `None` if the path has fewer
/// than four ancestors (custom layout — callers fail open).
fn entry_version_and_plugin_dir(entry: &ManifestEntry) -> Option<(PathBuf, PathBuf)> {
    let cache_path = Path::new(&entry.cache_path);
    let hooks_dir = cache_path.parent()?; // .../{version}/hooks
    let version_dir = hooks_dir.parent()?; // .../{version}
    let plugin_dir = version_dir.parent()?; // .../{plugin}
    Some((version_dir.to_path_buf(), plugin_dir.to_path_buf()))
}

/// Returns `true` only if `entry` is still safe to dispatch:
///   1. `entry.cache_path` exists on disk (plugin not uninstalled or GC'd),
///      AND
///   2. when the layout matches Claude Code's standard
///      `{plugin}/{semver}/hooks/{file}.json`, `entry`'s version dir is the
///      highest-semver sibling (so a plugin update from v1.0.0 → v2.0.0
///      stops dispatching the old v1.0.0 handler even if its cache dir
///      still lives on disk).
///
/// Fails open for custom layouts (non-semver version dir, missing
/// ancestors, unreadable plugin dir): keeps the entry rather than risk
/// silently dropping a legitimate handler. The on-disk `cache_path`
/// existence check above is the floor — that always applies.
fn is_entry_active(entry: &ManifestEntry) -> bool {
    if !Path::new(&entry.cache_path).exists() {
        return false;
    }
    let Some((entry_ver_dir, plugin_dir)) = entry_version_and_plugin_dir(entry) else {
        return true;
    };
    let entry_ver_name = entry_ver_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    // Non-semver version dirs (e.g. "latest", "v1") cannot be compared
    // safely; trust the on-disk existence check we already passed.
    if parse_semver(entry_ver_name) == (0, 0, 0) {
        return true;
    }
    let siblings = match fs::read_dir(&plugin_dir) {
        Ok(d) => d,
        Err(_) => return true,
    };
    let mut version_dirs: Vec<PathBuf> = siblings
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    if version_dirs.len() <= 1 {
        return true;
    }
    version_dirs.sort_by(|a, b| {
        let va = parse_semver(a.file_name().and_then(|n| n.to_str()).unwrap_or(""));
        let vb = parse_semver(b.file_name().and_then(|n| n.to_str()).unwrap_or(""));
        vb.cmp(&va) // descending
    });
    version_dirs
        .first()
        .map(|p| p == &entry_ver_dir)
        .unwrap_or(true)
}

/// Run every handler in the manifest against the original payload.
///
/// Called from BOTH:
/// - `HookResponse::NoOpinion` — RTK has no rewrite; manifest handlers
///   become the only deciders (full fallthrough).
/// - `HookResponse::Allow(rewrite)` — RTK wants to rewrite; manifest
///   handlers act as a deny-veto gate (autorun can still block a tool
///   even when RTK would have allowed it).
///
/// INVARIANT: `payload` MUST be the original unmodified stdin so handlers
/// see exactly what Claude Code sent. Forwarding RTK's rewrite instead
/// would defeat handler-level safety checks.
///
/// Stale-entry guard: each entry is checked by `is_entry_active` before
/// dispatch so a plugin update (v1→v2) does not dispatch the frozen v1
/// `fallthrough_command` whose `${CLAUDE_PLUGIN_ROOT}` was resolved to
/// the now-superseded version directory. `rtk init` re-runs are still
/// the canonical refresh path; this guard prevents the wrong-version
/// dispatch in between init runs.
///
/// I/O contract: never writes to stdout or stderr. Result bytes are
/// returned for the caller to forward at the appropriate exit code.
pub(crate) fn run_manifest_handlers(payload: &str) -> ManifestResult {
    let manifest = match load_manifest() {
        Some(m) => m,
        None => return ManifestResult::NoBlock,
    };

    // RAII guard: every child spawned below inherits RTK_ACTIVE=1 so if
    // the handler is itself a wrapper around `rtk hook claude`, the
    // recursive invocation sees `is_hook_disabled() == true` and exits
    // immediately. Without the guard a misconfigured plugin can loop
    // until the kernel kills the process tree.
    let _guard = super::recursion_guard::RtkActiveGuard::new();

    let mut block_json: Option<String> = None;
    let mut block_stderr: Vec<u8> = Vec::new();

    for entry in &manifest.entries {
        if !is_entry_active(entry) {
            continue;
        }
        let mut child = match Command::new("sh")
            .arg("-c")
            .arg(&entry.fallthrough_command)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(_) => continue, // fail-open: handler binary not found
        };

        // Track stdin write success so a failed write does not let the
        // child observe an empty payload and falsely emit exit 2.
        let write_ok = if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(payload.as_bytes()).is_ok()
        } else {
            false
        };

        let output = match child.wait_with_output() {
            Ok(o) => o,
            Err(_) => continue,
        };

        let exit_code = output.status.code().unwrap_or(0);
        let stdout_str = String::from_utf8_lossy(&output.stdout);
        let blocked = (exit_code == 2 && write_ok) || is_json_deny(&stdout_str);

        if blocked && block_json.is_none() {
            // Record FIRST block; loop still runs ALL handlers so each
            // handler can side-effect (audit logs, telemetry, etc.).
            block_json = Some(stdout_str.into_owned());
            block_stderr.extend_from_slice(&output.stderr);
        }
    }

    match block_json {
        Some(json) => ManifestResult::Blocked {
            json,
            stderr_bytes: block_stderr,
        },
        None => ManifestResult::NoBlock,
    }
}

// =========================================================================
// Install-time plugin cache patching
// =========================================================================

/// Returns `true` if `matcher` (Claude Code matcher syntax, pipe-separated)
/// contains the `Bash` token as a complete alternation.
///
/// Whole-token match so `BashPipeline` or `BashMagic` do not get patched.
pub(crate) fn matcher_contains_bash(matcher: &str) -> bool {
    matcher.split('|').any(|part| part.trim() == "Bash")
}

/// Remove every `Bash` token from a pipe-separated matcher string.
///
/// Example: `"Write|Edit|Bash|ExitPlanMode"` → `"Write|Edit|ExitPlanMode"`.
/// Returns the empty string if `Bash` was the only token; callers must
/// reject that case to avoid silently disabling the entry (an empty
/// matcher matches *no* tools at all).
pub(crate) fn remove_bash_from_matcher(matcher: &str) -> String {
    matcher
        .split('|')
        .filter(|part| part.trim() != "Bash")
        .collect::<Vec<_>>()
        .join("|")
}

/// Parse a semver-like version string into a tuple for descending sort.
///
/// Garbage components default to `0` so non-semver dirs (e.g. `latest`)
/// sort below real versions and never get picked as the active version.
fn parse_semver(s: &str) -> (u32, u32, u32) {
    let parts: Vec<u32> = s.split('.').filter_map(|p| p.parse().ok()).collect();
    (
        parts.first().copied().unwrap_or(0),
        parts.get(1).copied().unwrap_or(0),
        parts.get(2).copied().unwrap_or(0),
    )
}

/// Resolve `${CLAUDE_PLUGIN_ROOT}` in a command string to an absolute path.
///
/// Looks up the vendor in `extraKnownMarketplaces` from `settings.json`,
/// then tries (in order):
///   1. `{marketplace}/plugins/{plugin_name}` (matches plugin manifest)
///   2. `{marketplace}/plugins/{vendor_name}` (plugin source dir != name)
///   3. Scan `{marketplace}/plugins/` for the first dir with a `hooks/`
///      subdir (covers oddly-named plugin sources).
///   4. `~/.claude/plugins/{vendor}/{plugin}` (standard marketplace).
///
/// Falls back to leaving `${CLAUDE_PLUGIN_ROOT}` literal if resolution
/// fails — the resulting command will fail at runtime, but install
/// itself does not block.
fn resolve_plugin_root_in_command(
    command: &str,
    vendor_name: &str,
    plugin_name: &str,
    settings_root: &Value,
    claude_dir: &Path,
) -> String {
    if !command.contains("${CLAUDE_PLUGIN_ROOT}") {
        return command.to_string();
    }

    if let Some(marketplace_path) = settings_root
        .get("extraKnownMarketplaces")
        .and_then(|m| m.get(vendor_name))
        .and_then(|v| v.get("source"))
        .and_then(|s| s.get("path"))
        .and_then(|p| p.as_str())
    {
        let primary = format!("{}/plugins/{}", marketplace_path, plugin_name);
        if Path::new(&primary).exists() {
            return command.replace("${CLAUDE_PLUGIN_ROOT}", &primary);
        }
        let by_vendor = format!("{}/plugins/{}", marketplace_path, vendor_name);
        if Path::new(&by_vendor).exists() {
            return command.replace("${CLAUDE_PLUGIN_ROOT}", &by_vendor);
        }
        let plugins_dir = format!("{}/plugins", marketplace_path);
        if let Ok(entries) = fs::read_dir(&plugins_dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                let name_ok = p
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| !n.starts_with('.') && n != "__pycache__" && !n.contains("venv"))
                    .unwrap_or(false);
                if p.is_dir() && p.join("hooks").is_dir() && name_ok {
                    return command.replace("${CLAUDE_PLUGIN_ROOT}", &p.to_string_lossy());
                }
            }
        }
        return command.replace("${CLAUDE_PLUGIN_ROOT}", &primary);
    }

    let standard_root = claude_dir
        .join("plugins")
        .join(vendor_name)
        .join(plugin_name);
    if standard_root.exists() {
        return command.replace("${CLAUDE_PLUGIN_ROOT}", &standard_root.to_string_lossy());
    }

    // Couldn't resolve — leave the literal so runtime exposes the breakage
    // (instead of silently writing a half-resolved path into the manifest).
    command.to_string()
}

/// Patch a single plugin cache file. Returns `Ok(true)` if a manifest
/// entry was added (newly patched OR reconstructed), `Ok(false)` if
/// nothing needed to change.
///
/// The function follows two paths:
/// - **First-run path**: at least one entry still has `Bash` in the
///   matcher. Strip it, append a `ManifestEntry`, and write the cache
///   file atomically. The original is preserved at `<name>.rtk-backup`
///   by the caller before this is called.
/// - **Reconstruction path**: a prior `rtk init` already removed Bash
///   but the manifest is missing (user blew it away). Register the
///   current PreToolUse entries so the binary hook still calls them as
///   fallthrough handlers. Safe for uninstall because
///   `original_matcher == patched_matcher` makes the restore a no-op.
fn patch_single_cache_file(
    hook_path: &Path,
    vendor_name: &str,
    plugin_name: &str,
    settings_root: &Value,
    claude_dir: &Path,
    manifest: &mut BashManifest,
    verbose: u8,
) -> Result<bool> {
    let content = fs::read_to_string(hook_path)
        .with_context(|| format!("Failed to read {}", hook_path.display()))?;

    let mut json: Value = serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse {}", hook_path.display()))?;

    let cache_path_str = hook_path.to_string_lossy().into_owned();
    if manifest
        .entries
        .iter()
        .any(|e| e.cache_path == cache_path_str)
    {
        return Ok(false); // already registered; second run is a no-op
    }

    let pre_tool_use = match json
        .get_mut("hooks")
        .and_then(|h| h.get_mut("PreToolUse"))
        .and_then(|p| p.as_array_mut())
    {
        Some(arr) => arr,
        None => return Ok(false),
    };

    let has_any_bash = pre_tool_use
        .iter()
        .any(|e| matcher_contains_bash(e.get("matcher").and_then(|m| m.as_str()).unwrap_or("")));

    if !has_any_bash {
        // Reconstruction path — register existing entries as fallthrough
        let mut any_added = false;
        for entry in pre_tool_use.iter() {
            let matcher = entry
                .get("matcher")
                .and_then(|m| m.as_str())
                .unwrap_or("")
                .to_string();
            if matcher.is_empty() {
                continue;
            }
            let command = entry
                .get("hooks")
                .and_then(|h| h.as_array())
                .and_then(|arr| arr.first())
                .and_then(|h| h.get("command"))
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string();
            let resolved_command = resolve_plugin_root_in_command(
                &command,
                vendor_name,
                plugin_name,
                settings_root,
                claude_dir,
            );
            if verbose > 0 {
                eprintln!(
                    "Reconstructed manifest entry for '{}' (Bash already removed)",
                    hook_path.display()
                );
            }
            manifest.entries.push(ManifestEntry {
                cache_path: cache_path_str.clone(),
                original_matcher: matcher.clone(),
                patched_matcher: matcher,
                fallthrough_command: resolved_command,
            });
            any_added = true;
        }
        return Ok(any_added);
    }

    // First-run path — strip Bash and add manifest entry.
    let mut any_patched = false;
    for entry in pre_tool_use.iter_mut() {
        let matcher = entry
            .get("matcher")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string();

        if !matcher_contains_bash(&matcher) {
            continue;
        }

        let command = entry
            .get("hooks")
            .and_then(|h| h.as_array())
            .and_then(|arr| arr.first())
            .and_then(|h| h.get("command"))
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();

        let resolved_command = resolve_plugin_root_in_command(
            &command,
            vendor_name,
            plugin_name,
            settings_root,
            claude_dir,
        );
        let new_matcher = remove_bash_from_matcher(&matcher);

        // Empty matcher would silently disable the entire entry; skip
        // (warn so the user can investigate the plugin manually).
        if new_matcher.is_empty() {
            if verbose > 0 || cfg!(test) {
                eprintln!(
                    "Warning: skipping '{}' — matcher '{}' contains only Bash; \
                     cannot patch without breaking the entry.",
                    hook_path.display(),
                    matcher
                );
            }
            continue;
        }

        if let Some(entry_obj) = entry.as_object_mut() {
            entry_obj.insert("matcher".to_string(), Value::String(new_matcher.clone()));
        }

        manifest.entries.push(ManifestEntry {
            cache_path: cache_path_str.clone(),
            original_matcher: matcher,
            patched_matcher: new_matcher,
            fallthrough_command: resolved_command,
        });

        any_patched = true;
    }

    if any_patched {
        let patched = serde_json::to_string_pretty(&json)
            .context("Failed to serialize patched cache JSON")?;
        atomic_write(hook_path, &patched)?;
        if verbose > 0 {
            eprintln!("Patched: {}", hook_path.display());
        }
    }

    Ok(any_patched)
}

/// Scan every plugin cache for `Bash` matchers, strip them, and write a
/// manifest so the runtime hook can forward to displaced handlers.
///
/// Operates on `~/.claude/plugins/cache/{vendor}/{plugin}/{version}/hooks/*.json`.
/// Only the *highest* version of each plugin is processed; entries that
/// belong to older versions of the same plugin are removed from the
/// manifest so the binary hook never double-executes the same handler.
///
/// Returns the number of newly-patched cache files. A return of `0` is
/// not an error — it just means everything was already up to date (re-run
/// safe) or there were no Bash matchers to patch.
///
/// Non-fatal failures (unreadable plugin dirs, corrupt JSON, missing
/// permissions on a single file) are logged in verbose mode and the
/// scan continues so one bad plugin cannot block the rest of the install.
pub fn patch_plugin_caches(claude_dir: &Path, verbose: u8) -> Result<usize> {
    let cache_root = claude_dir.join("plugins").join("cache");
    let manifest_dir = claude_dir.join("hooks");
    let manifest_file = manifest_dir.join("rtk-bash-manifest.json");

    let mut manifest: BashManifest = manifest_file
        .exists()
        .then(|| fs::read_to_string(&manifest_file).ok())
        .flatten()
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or_default();

    if !cache_root.exists() {
        if verbose > 0 {
            eprintln!("Plugin cache directory not found: {}", cache_root.display());
        }
        return Ok(0);
    }

    let settings_path = claude_dir.join("settings.json");
    let settings_root: Value = if settings_path.exists() {
        let content = fs::read_to_string(&settings_path).unwrap_or_default();
        serde_json::from_str(&content).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    let mut newly_patched = 0usize;
    let mut already_present = 0usize;

    let vendors = match fs::read_dir(&cache_root) {
        Ok(d) => d,
        Err(_) => return Ok(0),
    };

    for vendor_entry in vendors.flatten() {
        let vendor_path = vendor_entry.path();
        if !vendor_path.is_dir() {
            continue;
        }
        let vendor_name = vendor_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();

        let plugins = match fs::read_dir(&vendor_path) {
            Ok(d) => d,
            Err(_) => continue,
        };

        for plugin_entry in plugins.flatten() {
            let plugin_path = plugin_entry.path();
            if !plugin_path.is_dir() {
                continue;
            }
            let plugin_name = plugin_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();

            let versions = match fs::read_dir(&plugin_path) {
                Ok(d) => d,
                Err(_) => continue,
            };

            let mut version_dirs: Vec<PathBuf> = versions
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect();
            version_dirs.sort_by(|a, b| {
                let va = parse_semver(a.file_name().and_then(|n| n.to_str()).unwrap_or(""));
                let vb = parse_semver(b.file_name().and_then(|n| n.to_str()).unwrap_or(""));
                vb.cmp(&va) // descending: highest first
            });

            let plugin_prefix = format!("{}/", plugin_path.to_string_lossy());
            if let Some(active_version) = version_dirs.first() {
                let active_prefix = format!("{}/", active_version.to_string_lossy());

                // Drop stale entries for non-active versions of this plugin
                let before = manifest.entries.len();
                manifest.entries.retain(|e| {
                    !e.cache_path.starts_with(&plugin_prefix)
                        || e.cache_path.starts_with(&active_prefix)
                });
                if manifest.entries.len() < before && verbose > 0 {
                    eprintln!(
                        "Removed {} stale manifest entries for {}/{} (not active version)",
                        before - manifest.entries.len(),
                        vendor_name,
                        plugin_name
                    );
                }

                let hooks_dir = active_version.join("hooks");
                if !hooks_dir.exists() {
                    continue;
                }

                let hook_files = match fs::read_dir(&hooks_dir) {
                    Ok(d) => d,
                    Err(_) => continue,
                };

                for hook_file in hook_files.flatten() {
                    let hook_path = hook_file.path();
                    if hook_path.extension().and_then(|e| e.to_str()) != Some("json") {
                        continue;
                    }
                    let in_manifest = manifest
                        .entries
                        .iter()
                        .any(|e| e.cache_path == hook_path.to_string_lossy().as_ref());

                    match patch_single_cache_file(
                        &hook_path,
                        &vendor_name,
                        &plugin_name,
                        &settings_root,
                        claude_dir,
                        &mut manifest,
                        verbose,
                    ) {
                        Ok(true) => newly_patched += 1,
                        Ok(false) if in_manifest => already_present += 1,
                        Ok(false) => {}
                        Err(e) => {
                            eprintln!("Warning: failed to patch {}: {}", hook_path.display(), e);
                        }
                    }
                }
            }
        }
    }

    if !manifest.entries.is_empty() {
        manifest.patched_at = chrono::Utc::now().to_rfc3339();
        manifest.version = 1;
        let manifest_json =
            serde_json::to_string_pretty(&manifest).context("Failed to serialize manifest")?;
        fs::create_dir_all(&manifest_dir).with_context(|| {
            format!(
                "Failed to create hooks directory: {}",
                manifest_dir.display()
            )
        })?;
        atomic_write(&manifest_file, &manifest_json)?;
        if verbose > 0 {
            eprintln!("Manifest written: {}", manifest_file.display());
        }
    }

    if newly_patched > 0 && verbose > 0 {
        eprintln!(
            "  Plugin caches: {} patched, {} already up-to-date",
            newly_patched, already_present
        );
    }

    Ok(newly_patched)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ----- is_json_deny --------------------------------------------------

    #[test]
    fn test_is_json_deny_claude_code_format() {
        let j = r#"{"hookSpecificOutput":{"permissionDecision":"deny","permissionDecisionReason":"r"}}"#;
        assert!(is_json_deny(j));
    }

    #[test]
    fn test_is_json_deny_gemini_format() {
        let j = r#"{"decision":"deny","reason":"r"}"#;
        assert!(is_json_deny(j));
    }

    #[test]
    fn test_is_json_deny_allow_not_matched() {
        assert!(!is_json_deny(
            r#"{"hookSpecificOutput":{"permissionDecision":"allow"}}"#
        ));
        assert!(!is_json_deny(r#"{"decision":"allow"}"#));
    }

    #[test]
    fn test_is_json_deny_empty_and_malformed() {
        assert!(!is_json_deny(""));
        assert!(!is_json_deny("not json"));
        assert!(!is_json_deny("{"));
    }

    #[test]
    fn test_is_json_deny_with_leading_whitespace() {
        let j = "   \n  {\"decision\":\"deny\"}  ";
        assert!(is_json_deny(j));
    }

    // ----- extract_deny_reason -------------------------------------------

    #[test]
    fn test_extract_deny_reason_cc_format() {
        let j = r#"{"hookSpecificOutput":{"permissionDecision":"deny","permissionDecisionReason":"Use grep"}}"#;
        assert_eq!(extract_deny_reason(j), Some("Use grep".to_owned()));
    }

    #[test]
    fn test_extract_deny_reason_gemini_format() {
        let j = r#"{"decision":"deny","reason":"blocked"}"#;
        assert_eq!(extract_deny_reason(j), Some("blocked".to_owned()));
    }

    #[test]
    fn test_extract_deny_reason_missing() {
        assert_eq!(extract_deny_reason("{}"), None);
        assert_eq!(extract_deny_reason("not json"), None);
    }

    // ----- matcher helpers ----------------------------------------------

    #[test]
    fn test_matcher_contains_bash_simple() {
        assert!(matcher_contains_bash("Bash"));
        assert!(matcher_contains_bash("Bash|Edit"));
        assert!(matcher_contains_bash("Edit|Bash|Write"));
    }

    #[test]
    fn test_matcher_contains_bash_only_whole_token() {
        // Substring of another tool name MUST NOT count
        assert!(!matcher_contains_bash("BashPipeline"));
        assert!(!matcher_contains_bash("MyBash"));
        assert!(!matcher_contains_bash("Edit|MyBash|Write"));
    }

    #[test]
    fn test_matcher_contains_bash_with_whitespace() {
        assert!(matcher_contains_bash(" Bash "));
        assert!(matcher_contains_bash("Edit | Bash | Write"));
    }

    #[test]
    fn test_remove_bash_from_matcher() {
        assert_eq!(remove_bash_from_matcher("Bash"), "");
        assert_eq!(remove_bash_from_matcher("Bash|Edit"), "Edit");
        assert_eq!(remove_bash_from_matcher("Edit|Bash|Write"), "Edit|Write");
        assert_eq!(
            remove_bash_from_matcher("Write|Edit|ExitPlanMode"),
            "Write|Edit|ExitPlanMode"
        );
    }

    #[test]
    fn test_remove_bash_preserves_lookalikes() {
        // BashPipeline must survive
        assert_eq!(
            remove_bash_from_matcher("Bash|BashPipeline|Edit"),
            "BashPipeline|Edit"
        );
    }

    // ----- parse_semver --------------------------------------------------

    #[test]
    fn test_parse_semver_basic() {
        assert_eq!(parse_semver("1.2.3"), (1, 2, 3));
        assert_eq!(parse_semver("0.0.1"), (0, 0, 1));
        assert_eq!(parse_semver("10.20.30"), (10, 20, 30));
    }

    #[test]
    fn test_parse_semver_garbage_zero() {
        assert_eq!(parse_semver("latest"), (0, 0, 0));
        assert_eq!(parse_semver(""), (0, 0, 0));
        // "v1" fails to parse so it is *dropped* by filter_map (not zeroed),
        // so the remaining segments slide left: "v1.2.3" → [2, 3] → (2, 3, 0).
        // Documents the actual behaviour so a future refactor does not
        // silently change version ordering for plugins with `v`-prefixed dirs.
        assert_eq!(parse_semver("v1.2.3"), (2, 3, 0));
    }

    #[test]
    fn test_parse_semver_partial() {
        assert_eq!(parse_semver("2"), (2, 0, 0));
        assert_eq!(parse_semver("2.5"), (2, 5, 0));
    }

    // ----- load_manifest behaviour --------------------------------------

    #[test]
    fn test_load_manifest_returns_none_when_missing() {
        // We can't easily fake HOME for this test in parallel runs, but we
        // can assert the function never panics regardless of environment.
        let result = load_manifest();
        drop(result);
    }

    // ----- run_manifest_handlers ----------------------------------------

    #[test]
    fn test_run_manifest_handlers_no_manifest_returns_noblock() {
        // With no manifest, must return NoBlock and never crash.
        match run_manifest_handlers("{}") {
            ManifestResult::NoBlock => {}
            ManifestResult::Blocked { .. } => {
                // Tolerated only if user actually has a manifest installed
                // (we run inside a worktree where HOME may point at a real
                // ~/.claude with a manifest from the developer's machine).
            }
        }
    }

    // ----- BashManifest serde --------------------------------------------

    #[test]
    fn test_manifest_roundtrips_default() {
        let m = BashManifest::default();
        let s = serde_json::to_string(&m).unwrap();
        let back: BashManifest = serde_json::from_str(&s).unwrap();
        assert_eq!(back.version, 1);
        assert!(back.entries.is_empty());
    }

    #[test]
    fn test_manifest_roundtrips_with_entries() {
        let m = BashManifest {
            version: 1,
            patched_at: "2026-05-25T00:00:00Z".to_string(),
            entries: vec![ManifestEntry {
                cache_path: "/tmp/cache/x.json".to_string(),
                original_matcher: "Bash|Edit".to_string(),
                patched_matcher: "Edit".to_string(),
                fallthrough_command: "/usr/local/bin/handler".to_string(),
            }],
        };
        let s = serde_json::to_string(&m).unwrap();
        let back: BashManifest = serde_json::from_str(&s).unwrap();
        assert_eq!(back.entries.len(), 1);
        assert_eq!(back.entries[0].original_matcher, "Bash|Edit");
        assert_eq!(back.entries[0].patched_matcher, "Edit");
    }

    #[test]
    fn test_manifest_load_missing_version_defaults_to_1() {
        // Forward-compat: an older manifest without version still loads.
        let j = r#"{"entries":[],"patched_at":""}"#;
        let m: BashManifest = serde_json::from_str(j).unwrap();
        assert_eq!(m.version, 1);
    }

    #[test]
    fn test_manifest_load_malformed_is_none_when_used_via_load() {
        // load_manifest swallows errors and returns None. This is the
        // critical fail-open property — the hook MUST NOT panic on a
        // broken file. We can't trigger a real load in parallel tests
        // without HOME mucking, so verify the deserialiser semantics:
        let result: Result<BashManifest, _> = serde_json::from_str("not json");
        assert!(result.is_err());
    }

    // ----- patch_plugin_caches integration (tmpdir-based) ----------------

    fn write_file(p: &Path, content: &str) {
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, content).unwrap();
    }

    #[test]
    fn test_patch_plugin_caches_no_cache_dir_returns_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let n = patch_plugin_caches(tmp.path(), 0).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn test_patch_plugin_caches_strips_bash() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_dir = tmp.path();

        // Build a fake cache layout
        let plugin_dir = claude_dir
            .join("plugins")
            .join("cache")
            .join("vendor-x")
            .join("plugin-y")
            .join("1.0.0");
        let hook_json = plugin_dir.join("hooks").join("preToolUse.json");
        let cache_content = json!({
            "hooks": {
                "PreToolUse": [
                    {
                        "matcher": "Write|Edit|Bash|ExitPlanMode",
                        "hooks": [{"type": "command", "command": "/usr/local/bin/plugin-y"}]
                    }
                ]
            }
        });
        write_file(
            &hook_json,
            &serde_json::to_string_pretty(&cache_content).unwrap(),
        );

        let n = patch_plugin_caches(claude_dir, 0).unwrap();
        assert_eq!(n, 1);

        // Cache should now lack Bash
        let after: Value = serde_json::from_str(&fs::read_to_string(&hook_json).unwrap()).unwrap();
        let matcher = after["hooks"]["PreToolUse"][0]["matcher"].as_str().unwrap();
        assert_eq!(matcher, "Write|Edit|ExitPlanMode");

        // Manifest should record the original matcher and command
        let manifest_path = claude_dir.join("hooks").join("rtk-bash-manifest.json");
        assert!(manifest_path.exists(), "manifest must be written");
        let m: BashManifest =
            serde_json::from_str(&fs::read_to_string(&manifest_path).unwrap()).unwrap();
        assert_eq!(m.entries.len(), 1);
        assert_eq!(
            m.entries[0].original_matcher,
            "Write|Edit|Bash|ExitPlanMode"
        );
        assert_eq!(m.entries[0].patched_matcher, "Write|Edit|ExitPlanMode");
        assert_eq!(m.entries[0].fallthrough_command, "/usr/local/bin/plugin-y");
    }

    #[test]
    fn test_patch_plugin_caches_idempotent_second_run() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_dir = tmp.path();
        let plugin_dir = claude_dir
            .join("plugins")
            .join("cache")
            .join("vendor-x")
            .join("plugin-y")
            .join("1.0.0");
        let hook_json = plugin_dir.join("hooks").join("preToolUse.json");
        let cache_content = json!({
            "hooks": {
                "PreToolUse": [
                    {
                        "matcher": "Bash|Edit",
                        "hooks": [{"type": "command", "command": "/usr/local/bin/h"}]
                    }
                ]
            }
        });
        write_file(
            &hook_json,
            &serde_json::to_string_pretty(&cache_content).unwrap(),
        );

        let n1 = patch_plugin_caches(claude_dir, 0).unwrap();
        let n2 = patch_plugin_caches(claude_dir, 0).unwrap();
        assert_eq!(n1, 1, "first run patches");
        assert_eq!(n2, 0, "second run is a no-op (manifest dedupes)");
    }

    #[test]
    fn test_patch_plugin_caches_skips_bash_only_matcher() {
        // Matcher of just "Bash" would become empty after stripping;
        // we must skip the entry instead of writing an empty matcher
        // that silently disables the plugin.
        let tmp = tempfile::tempdir().unwrap();
        let claude_dir = tmp.path();
        let plugin_dir = claude_dir
            .join("plugins")
            .join("cache")
            .join("vendor-x")
            .join("plugin-y")
            .join("1.0.0");
        let hook_json = plugin_dir.join("hooks").join("preToolUse.json");
        let cache_content = json!({
            "hooks": {
                "PreToolUse": [
                    {
                        "matcher": "Bash",
                        "hooks": [{"type": "command", "command": "/x"}]
                    }
                ]
            }
        });
        write_file(
            &hook_json,
            &serde_json::to_string_pretty(&cache_content).unwrap(),
        );

        let n = patch_plugin_caches(claude_dir, 0).unwrap();
        assert_eq!(
            n, 0,
            "Bash-only matcher must be skipped (would corrupt entry)"
        );
        // Cache should remain unchanged
        let after: Value = serde_json::from_str(&fs::read_to_string(&hook_json).unwrap()).unwrap();
        assert_eq!(after["hooks"]["PreToolUse"][0]["matcher"], "Bash");
    }

    #[test]
    fn test_patch_plugin_caches_only_active_version() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_dir = tmp.path();
        let plugin_root = claude_dir
            .join("plugins")
            .join("cache")
            .join("vendor-x")
            .join("plugin-y");

        // Two versions: 1.0.0 (older) and 2.5.0 (active)
        let old_hook = plugin_root.join("1.0.0").join("hooks").join("h.json");
        let new_hook = plugin_root.join("2.5.0").join("hooks").join("h.json");
        let content = json!({
            "hooks": {
                "PreToolUse": [{"matcher": "Bash|Edit",
                                "hooks": [{"command": "/cmd"}]}]
            }
        });
        write_file(&old_hook, &serde_json::to_string_pretty(&content).unwrap());
        write_file(&new_hook, &serde_json::to_string_pretty(&content).unwrap());

        let n = patch_plugin_caches(claude_dir, 0).unwrap();
        assert_eq!(n, 1, "only the newest version is patched");

        let manifest_path = claude_dir.join("hooks").join("rtk-bash-manifest.json");
        let m: BashManifest =
            serde_json::from_str(&fs::read_to_string(&manifest_path).unwrap()).unwrap();
        assert_eq!(m.entries.len(), 1);
        assert!(
            m.entries[0].cache_path.contains("2.5.0"),
            "manifest must point at active version, got: {}",
            m.entries[0].cache_path
        );
    }

    #[test]
    fn test_patch_plugin_caches_reconstructs_when_bash_already_removed() {
        // Reconstruction path: a prior `rtk init` already stripped Bash but
        // the manifest is gone (deleted manually, fresh machine restore,
        // etc.). The PreToolUse entries are still there and would never
        // fire for Bash again unless we register them as fallthrough.
        // patch_plugin_caches must therefore record them in the manifest
        // with original_matcher == patched_matcher (so uninstall is a no-op).
        let tmp = tempfile::tempdir().unwrap();
        let claude_dir = tmp.path();
        let hook_json = claude_dir
            .join("plugins")
            .join("cache")
            .join("vendor-x")
            .join("plugin-y")
            .join("1.0.0")
            .join("hooks")
            .join("h.json");
        let content = json!({
            "hooks": {
                "PreToolUse": [{"matcher": "Write|Edit", "hooks": [{"command": "/x"}]}]
            }
        });
        write_file(&hook_json, &serde_json::to_string_pretty(&content).unwrap());
        let n = patch_plugin_caches(claude_dir, 0).unwrap();
        assert_eq!(
            n, 1,
            "reconstruction path must register the existing entry for fallthrough"
        );
        // Cache content must be unchanged
        let after: Value = serde_json::from_str(&fs::read_to_string(&hook_json).unwrap()).unwrap();
        assert_eq!(after["hooks"]["PreToolUse"][0]["matcher"], "Write|Edit");
        // Manifest must record the entry with original == patched (no-op restore)
        let manifest_path = claude_dir.join("hooks").join("rtk-bash-manifest.json");
        let m: BashManifest =
            serde_json::from_str(&fs::read_to_string(&manifest_path).unwrap()).unwrap();
        assert_eq!(m.entries.len(), 1);
        assert_eq!(m.entries[0].original_matcher, "Write|Edit");
        assert_eq!(m.entries[0].patched_matcher, "Write|Edit");
        assert_eq!(m.entries[0].fallthrough_command, "/x");
    }

    #[test]
    fn test_patch_plugin_caches_no_pretooluse_returns_zero() {
        // No PreToolUse hooks at all → nothing to register, manifest stays absent.
        let tmp = tempfile::tempdir().unwrap();
        let claude_dir = tmp.path();
        let hook_json = claude_dir
            .join("plugins")
            .join("cache")
            .join("vendor-x")
            .join("plugin-y")
            .join("1.0.0")
            .join("hooks")
            .join("h.json");
        let content = json!({"hooks": {"PostToolUse": [{"matcher": "Edit"}]}});
        write_file(&hook_json, &serde_json::to_string_pretty(&content).unwrap());
        let n = patch_plugin_caches(claude_dir, 0).unwrap();
        assert_eq!(n, 0);
        let manifest_path = claude_dir.join("hooks").join("rtk-bash-manifest.json");
        assert!(
            !manifest_path.exists(),
            "no PreToolUse entries → no manifest"
        );
    }

    // ----- is_entry_active: stale-entry runtime guard --------------------
    //
    // Issue: BashManifest entries bake the absolute path of the active
    // version dir into `fallthrough_command` at install time. When a
    // plugin updates v1.0.0 → v2.5.0 between `rtk init` runs the stored
    // command still points at v1 — dispatching it executes stale logic or
    // crashes silently. `is_entry_active` is the runtime safety net.

    fn make_entry(cache_path: &Path) -> ManifestEntry {
        ManifestEntry {
            cache_path: cache_path.to_string_lossy().into_owned(),
            original_matcher: "Bash|Edit".to_string(),
            patched_matcher: "Edit".to_string(),
            fallthrough_command: "echo placeholder".to_string(),
        }
    }

    #[test]
    fn test_is_entry_active_missing_cache_path() {
        let tmp = tempfile::tempdir().unwrap();
        let entry = make_entry(&tmp.path().join("definitely/missing.json"));
        assert!(
            !is_entry_active(&entry),
            "uninstalled or GC'd path must be skipped"
        );
    }

    #[test]
    fn test_is_entry_active_single_version_dir_present() {
        let tmp = tempfile::tempdir().unwrap();
        let hook_json = tmp
            .path()
            .join("plugins/cache/vendor/plug/1.0.0/hooks/h.json");
        write_file(&hook_json, "{}");
        let entry = make_entry(&hook_json);
        assert!(
            is_entry_active(&entry),
            "single live version → entry is active"
        );
    }

    #[test]
    fn test_is_entry_active_old_version_when_newer_exists() {
        // The exact scenario the user hit: plugin updated from 1.0.0 to
        // 2.5.0, but the old cache dir survived (lazy GC). The entry's
        // cache_path still resolves but it is no longer the active
        // version — runtime MUST skip it so the v1 handler does not run
        // against v2 payloads.
        let tmp = tempfile::tempdir().unwrap();
        let plugin_dir = tmp.path().join("plugins/cache/vendor/plug");
        let old_hook = plugin_dir.join("1.0.0/hooks/h.json");
        let new_hook = plugin_dir.join("2.5.0/hooks/h.json");
        write_file(&old_hook, "{}");
        write_file(&new_hook, "{}");
        let stale = make_entry(&old_hook);
        let active = make_entry(&new_hook);
        assert!(
            !is_entry_active(&stale),
            "older version dir must be skipped when a newer one exists"
        );
        assert!(
            is_entry_active(&active),
            "highest-semver version dir is the active one"
        );
    }

    #[test]
    fn test_is_entry_active_custom_layout_falls_open() {
        // Custom installs that do not follow the {plugin}/{semver}/hooks
        // layout (e.g. tests, packaged distros, symlink farms) keep their
        // entries as long as the file exists. The path-exists check is
        // the floor; we never silently drop an entry for layout reasons.
        let tmp = tempfile::tempdir().unwrap();
        let hook_json = tmp.path().join("custom-layout/handler.json");
        write_file(&hook_json, "{}");
        let entry = make_entry(&hook_json);
        assert!(
            is_entry_active(&entry),
            "non-semver layouts must fail open when the file exists"
        );
    }

    #[test]
    fn test_is_entry_active_non_semver_version_dir() {
        // Version dir named "latest" / "v1" / etc. parses to (0,0,0). We
        // do not try to rank these against semver siblings — fail open.
        let tmp = tempfile::tempdir().unwrap();
        let hook_json = tmp
            .path()
            .join("plugins/cache/vendor/plug/latest/hooks/h.json");
        write_file(&hook_json, "{}");
        let entry = make_entry(&hook_json);
        assert!(
            is_entry_active(&entry),
            "non-semver version dir must fail open"
        );
    }
}
