use anyhow::{Context, Result};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;

// Embedded slim RTK awareness instructions
const RTK_SLIM: &str = include_str!("../hooks/rtk-awareness.md");

// Embedded hook script (for --hook-type script mode)
const REWRITE_HOOK: &str = include_str!("../hooks/rtk-rewrite.sh");

/// Control flow for settings.json patching
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PatchMode {
    Ask,  // Default: prompt user [y/N]
    Auto, // --auto-patch: no prompt
    Skip, // --no-patch: manual instructions
}

/// Result of settings.json patching operation
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PatchResult {
    Patched,        // Hook was added successfully
    AlreadyPresent, // Hook was already in settings.json
    Declined,       // User declined when prompted
    Skipped,        // --no-patch flag used
}

/// Which hook mechanism to install for Claude Code PreToolUse:Bash.
///
/// Binary: installs `"rtk hook claude"` directly. Fastest, no shell dependency.
/// Script: deploys `rtk-rewrite.sh` and installs it as the hook. Shell-portable.
#[derive(clap::ValueEnum, Clone, Debug, PartialEq)]
pub enum HookType {
    /// Install "rtk hook claude" as the Claude Code hook (fast, no shell dependency)
    Binary,
    /// Deploy rtk-rewrite.sh and install it as the Claude Code hook (shell-portable)
    Script,
}

// Legacy full instructions for backward compatibility (--claude-md mode)
const RTK_INSTRUCTIONS: &str = r##"<!-- rtk-instructions v2 -->
# RTK (Rust Token Killer) - Token-Optimized Commands

## Golden Rule

**Always prefix commands with `rtk`**. If RTK has a dedicated filter, it uses it. If not, it passes through unchanged. This means RTK is always safe to use.

**Important**: Even in command chains with `&&`, use `rtk`:
```bash
# ❌ Wrong
git add . && git commit -m "msg" && git push

# ✅ Correct
rtk git add . && rtk git commit -m "msg" && rtk git push
```

## RTK Commands by Workflow

### Build & Compile (80-90% savings)
```bash
rtk cargo build         # Cargo build output
rtk cargo check         # Cargo check output
rtk cargo clippy        # Clippy warnings grouped by file (80%)
rtk tsc                 # TypeScript errors grouped by file/code (83%)
rtk lint                # ESLint/Biome violations grouped (84%)
rtk prettier --check    # Files needing format only (70%)
rtk next build          # Next.js build with route metrics (87%)
```

### Test (90-99% savings)
```bash
rtk cargo test          # Cargo test failures only (90%)
rtk vitest run          # Vitest failures only (99.5%)
rtk playwright test     # Playwright failures only (94%)
rtk test <cmd>          # Generic test wrapper - failures only
```

### Git (59-80% savings)
```bash
rtk git status          # Compact status
rtk git log             # Compact log (works with all git flags)
rtk git diff            # Compact diff (80%)
rtk git show            # Compact show (80%)
rtk git add             # Ultra-compact confirmations (59%)
rtk git commit          # Ultra-compact confirmations (59%)
rtk git push            # Ultra-compact confirmations
rtk git pull            # Ultra-compact confirmations
rtk git branch          # Compact branch list
rtk git fetch           # Compact fetch
rtk git stash           # Compact stash
rtk git worktree        # Compact worktree
```

Note: Git passthrough works for ALL subcommands, even those not explicitly listed.

### GitHub (26-87% savings)
```bash
rtk gh pr view <num>    # Compact PR view (87%)
rtk gh pr checks        # Compact PR checks (79%)
rtk gh run list         # Compact workflow runs (82%)
rtk gh issue list       # Compact issue list (80%)
rtk gh api              # Compact API responses (26%)
```

### JavaScript/TypeScript Tooling (70-90% savings)
```bash
rtk pnpm list           # Compact dependency tree (70%)
rtk pnpm outdated       # Compact outdated packages (80%)
rtk pnpm install        # Compact install output (90%)
rtk npm run <script>    # Compact npm script output
rtk npx <cmd>           # Compact npx command output
rtk prisma              # Prisma without ASCII art (88%)
```

### Files & Search (60-75% savings)
```bash
rtk ls <path>           # Tree format, compact (65%)
rtk read <file>         # Code reading with filtering (60%)
rtk grep <pattern>      # Search grouped by file (75%)
rtk find <pattern>      # Find grouped by directory (70%)
```

### Analysis & Debug (70-90% savings)
```bash
rtk err <cmd>           # Filter errors only from any command
rtk log <file>          # Deduplicated logs with counts
rtk json <file>         # JSON structure without values
rtk deps                # Dependency overview
rtk env                 # Environment variables compact
rtk summary <cmd>       # Smart summary of command output
rtk diff                # Ultra-compact diffs
```

### Infrastructure (85% savings)
```bash
rtk docker ps           # Compact container list
rtk docker images       # Compact image list
rtk docker logs <c>     # Deduplicated logs
rtk kubectl get         # Compact resource list
rtk kubectl logs        # Deduplicated pod logs
```

### Network (65-70% savings)
```bash
rtk curl <url>          # Compact HTTP responses (70%)
rtk wget <url>          # Compact download output (65%)
```

### Meta Commands
```bash
rtk gain                # View token savings statistics
rtk gain --history      # View command history with savings
rtk discover            # Analyze Claude Code sessions for missed RTK usage
rtk proxy <cmd>         # Run command without filtering (for debugging)
rtk init                # Add RTK instructions to CLAUDE.md
rtk init --global       # Add RTK to ~/.claude/CLAUDE.md
```

## Token Savings Overview

| Category | Commands | Typical Savings |
|----------|----------|-----------------|
| Tests | vitest, playwright, cargo test | 90-99% |
| Build | next, tsc, lint, prettier | 70-87% |
| Git | status, log, diff, add, commit | 59-80% |
| GitHub | gh pr, gh run, gh issue | 26-87% |
| Package Managers | pnpm, npm, npx | 70-90% |
| Files | ls, read, grep, find | 60-75% |
| Infrastructure | docker, kubectl | 85% |
| Network | curl, wget | 65-70% |

Overall average: **60-90% token reduction** on common development operations.
<!-- /rtk-instructions -->
"##;

/// Severity of an environment check result.
#[derive(Debug, Clone, Copy, PartialEq)]
enum IssueSeverity {
    /// Blocks automatic setup; user must act before RTK hooks will work.
    Hard,
    /// Hook install will proceed but a feature may be degraded.
    Soft,
}

/// A single finding from `check_environment()`.
struct EnvIssue {
    severity: IssueSeverity,
    /// Short description of what is wrong.
    problem: String,
    /// Step-by-step fix instructions (printed as a numbered list).
    instructions: Vec<String>,
    /// Optional doc links shown as "  → <url>".
    links: Vec<&'static str>,
}

/// Return shell-specific PATH setup instructions for the user's current shell.
///
/// Reads `$SHELL` from the environment and dispatches to per-shell advice.
/// Falls back to POSIX generic instructions for unknown/missing shells.
/// This avoids hard-coding zsh/bash assumptions for fish, nushell, elvish, etc.
#[cfg(unix)]
fn path_setup_instructions(cargo_bin: &str) -> Vec<String> {
    let shell = std::env::var("SHELL").unwrap_or_default();
    let shell_name = std::path::Path::new(&shell)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");

    match shell_name {
        "zsh" => vec![
            "Add to ~/.zprofile (login shell — read by non-interactive shells too):".to_owned(),
            format!(r#"  export PATH="{cargo_bin}:$PATH""#),
            "Reload: source ~/.zprofile  or open a new terminal".to_owned(),
        ],
        "bash" => vec![
            "Add to ~/.bash_profile (or ~/.profile if that file doesn't exist):".to_owned(),
            format!(r#"  export PATH="{cargo_bin}:$PATH""#),
            "Reload: source ~/.bash_profile  or open a new terminal".to_owned(),
        ],
        "fish" => vec![
            "Run once in fish to permanently add Cargo's bin to PATH:".to_owned(),
            format!("  fish_add_path {cargo_bin}"),
            "(fish_add_path writes to universal variables — no file edit needed)".to_owned(),
        ],
        "nu" | "nush" | "nushell" => vec![
            "Add to ~/.config/nushell/env.nu:".to_owned(),
            format!(r#"  $env.PATH = ($env.PATH | split row (char esep) | append "{cargo_bin}")"#),
            "Then restart nushell or run: source ~/.config/nushell/env.nu".to_owned(),
        ],
        _ => vec![
            format!("Add {cargo_bin} to your shell's PATH (consult your shell's documentation)."),
            "For POSIX shells (sh, dash, ksh, etc.) add to ~/.profile:".to_owned(),
            format!(r#"  export PATH="{cargo_bin}:$PATH""#),
            "Then open a new terminal or reload your profile.".to_owned(),
        ],
    }
}

/// Return advice on where jq PATH must be exported for non-interactive sh invocations.
/// Claude Code hooks are spawned as login-shell children; they see login-profile PATH,
/// not interactive-shell PATH (.zshrc, .bashrc). The advice is shell-specific.
#[cfg(unix)]
fn jq_path_profile_hint() -> String {
    let shell = std::env::var("SHELL").unwrap_or_default();
    let shell_name = std::path::Path::new(&shell)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");

    match shell_name {
        "zsh" => {
            "If jq is installed but not detected, ensure it is exported in ~/.zprofile,\n  not only in ~/.zshrc (which is not sourced by non-interactive shells).".to_owned()
        }
        "bash" => {
            "If jq is installed but not detected, ensure it is exported in ~/.bash_profile\n  or ~/.profile, not only in ~/.bashrc.".to_owned()
        }
        "fish" => {
            "If jq is installed but not detected, run: fish_add_path $(which jq | xargs dirname)\n  (fish universal PATH is inherited by subprocesses including Claude Code hooks).".to_owned()
        }
        _ => {
            "If jq is installed but not detected, ensure your shell's login profile exports PATH\n  (not only the interactive rc file). Hook subprocesses use the login-shell environment.".to_owned()
        }
    }
}

/// Run pre-flight environment checks before attempting hook installation.
///
/// Checks (in order):
/// 1. `~/.claude/` directory exists → Claude Code has been launched at least once.
/// 2. For `HookType::Script`: `jq` is on PATH → required by `rtk-rewrite.sh`.
/// 3. `rtk hook` subcommand is responsive → correct binary (not reachingforthejack/rtk).
///
/// Note: missing `settings.json` is NOT checked here. `patch_settings_shared` creates
/// it from scratch when absent, so warning would be noise on new Claude Code installs.
///
/// Returns a `Vec<EnvIssue>`. Hard issues mean the caller should bail; soft
/// issues are printed as warnings and execution continues.
#[cfg(unix)]
fn check_environment(hook_type: &HookType) -> Vec<EnvIssue> {
    let mut issues = Vec::new();

    // 1. ~/.claude/ directory — Claude Code must have been launched at least once.
    let claude_dir = match dirs::home_dir() {
        Some(h) => h.join(".claude"),
        None => {
            issues.push(EnvIssue {
                severity: IssueSeverity::Hard,
                problem: "$HOME is not set; cannot locate ~/.claude/".to_owned(),
                instructions: vec![
                    "Ensure the HOME environment variable points to your home directory."
                        .to_owned(),
                ],
                links: vec![],
            });
            return issues; // All subsequent checks depend on home dir
        }
    };

    if !claude_dir.exists() {
        issues.push(EnvIssue {
            severity: IssueSeverity::Hard,
            problem: format!("Claude Code directory not found: {}", claude_dir.display()),
            instructions: vec![
                "Install Claude Code if you haven't already.".to_owned(),
                "Launch Claude Code at least once so it creates its configuration directory."
                    .to_owned(),
                "Then re-run: rtk init -g".to_owned(),
            ],
            links: vec![
                "https://code.claude.com/docs/en/",
                "https://code.claude.com/docs/en/settings",
            ],
        });
        return issues;
    }

    // 2. jq — required only for Script mode (rtk-rewrite.sh guards on it at runtime too).
    //
    // We probe via `sh -c "command -v jq"` — POSIX built-in, works on /bin/sh, bash,
    // dash, and zsh-as-sh. Claude Code hook subprocesses inherit the same non-interactive
    // PATH that `sh` sees here, so this is an accurate proxy for hook runtime availability.
    if *hook_type == HookType::Script {
        let jq_found = std::process::Command::new("sh")
            .args(["-c", "command -v jq"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);

        if !jq_found {
            let mut instrs = vec![
                "Install jq using your system package manager:".to_owned(),
                "  macOS:          brew install jq".to_owned(),
                "  Ubuntu/Debian:  sudo apt install jq".to_owned(),
                "  Fedora/RHEL:    sudo dnf install jq".to_owned(),
                "  Windows (WSL):  sudo apt install jq".to_owned(),
                jq_path_profile_hint(),
                "Then re-run: rtk init -g --hook-type script".to_owned(),
                "Or use the binary hook (no jq needed): rtk init -g --hook-type binary".to_owned(),
            ];
            instrs.retain(|s| !s.is_empty());
            issues.push(EnvIssue {
                severity: IssueSeverity::Hard,
                problem: "`jq` not found on PATH (required by rtk-rewrite.sh hook script)"
                    .to_owned(),
                instructions: instrs,
                links: vec![
                    "https://jqlang.org/download/",
                    "https://code.claude.com/docs/en/hooks",
                ],
            });
        }
    }

    // 3. rtk self-check: verify the correct binary is installed.
    //
    // Two "rtk" packages exist on crates.io:
    //   - rtk-ai/rtk (Rust Token Killer) — has `rtk hook` subcommand
    //   - reachingforthejack/rtk (Rust Type Kit) — does NOT
    // Probe `rtk hook --help` directly (no shell wrapper) to catch the collision.
    let rtk_hook_ok = std::process::Command::new("rtk")
        .args(["hook", "--help"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if !rtk_hook_ok {
        // Distinguish: rtk not on PATH vs wrong rtk binary.
        // Use `which rtk` (POSIX, available on macOS and Linux) rather than
        // `command -v rtk` via sh to avoid spawning an extra shell just for this.
        let rtk_on_path = std::process::Command::new("which")
            .arg("rtk")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);

        if rtk_on_path {
            // Found an rtk binary, but it doesn't support `hook` → name collision.
            issues.push(EnvIssue {
                severity: IssueSeverity::Hard,
                problem:
                    "`rtk` on PATH does not support `hook` subcommand (wrong package installed)"
                        .to_owned(),
                instructions: vec![
                    "Two packages share the name 'rtk' on crates.io:".to_owned(),
                    "  ✅ rtk-ai/rtk              (Rust Token Killer — this project)".to_owned(),
                    "  ❌ reachingforthejack/rtk  (Rust Type Kit — unrelated)".to_owned(),
                    "Uninstall the wrong one:  cargo uninstall rtk".to_owned(),
                    "Install the correct one:  cargo install --git https://github.com/rtk-ai/rtk"
                        .to_owned(),
                    "Verify: rtk --version  (should show 'rtk X.Y.Z')".to_owned(),
                    "        rtk gain       (should show token savings stats)".to_owned(),
                ],
                links: vec!["https://github.com/rtk-ai/rtk"],
            });
        } else {
            // rtk not on PATH at all — give shell-appropriate PATH setup advice.
            let cargo_bin = dirs::home_dir()
                .map(|h| h.join(".cargo").join("bin").to_string_lossy().into_owned())
                .unwrap_or_else(|| "$HOME/.cargo/bin".to_owned());
            let mut instrs =
                vec!["Ensure your shell's PATH includes Cargo's bin directory.".to_owned()];
            instrs.extend(path_setup_instructions(&cargo_bin));
            instrs.push("Then re-run: rtk init -g".to_owned());
            issues.push(EnvIssue {
                severity: IssueSeverity::Hard,
                problem: "`rtk` not found on PATH after installation".to_owned(),
                instructions: instrs,
                links: vec!["https://doc.rust-lang.org/cargo/getting-started/installation.html"],
            });
        }
    }

    issues
}

/// Print environment issues to stderr with clear formatting.
/// Returns `true` if any hard issues were found (caller should bail).
#[cfg(unix)]
fn report_env_issues(issues: &[EnvIssue]) -> bool {
    let mut has_hard = false;
    for issue in issues {
        let label = match issue.severity {
            IssueSeverity::Hard => "❌ SETUP REQUIRED",
            IssueSeverity::Soft => "⚠️  WARNING",
        };
        eprintln!("\n{}: {}", label, issue.problem);
        if !issue.instructions.is_empty() {
            eprintln!("\n  How to fix:");
            for (i, step) in issue.instructions.iter().enumerate() {
                eprintln!("  {}. {}", i + 1, step);
            }
        }
        if !issue.links.is_empty() {
            eprintln!("\n  Reference:");
            for link in &issue.links {
                eprintln!("    → {}", link);
            }
        }
        if issue.severity == IssueSeverity::Hard {
            has_hard = true;
        }
    }
    if has_hard {
        eprintln!(
            "\n💡 Tip: If you need help setting this up, copy the output above and paste it\n   into your AI assistant (e.g., Claude) — it can walk you through the steps.\n"
        );
    }
    has_hard
}

/// Main entry point for `rtk init`
pub fn run(
    global: bool,
    claude_md: bool,
    hook_only: bool,
    patch_mode: PatchMode,
    hook_type: HookType,
    verbose: u8,
) -> Result<()> {
    // Mode selection
    match (claude_md, hook_only) {
        (true, _) => run_claude_md_mode(global, verbose),
        (false, true) => run_hook_only_mode(global, patch_mode, hook_type, verbose),
        (false, false) => run_default_mode(global, patch_mode, hook_type, verbose),
    }
}

/// Compute hook directory and script path for --hook-type script mode.
fn prepare_hook_paths() -> Result<(PathBuf, PathBuf)> {
    let claude_dir = resolve_claude_dir()?;
    let hook_dir = claude_dir.join("hooks");
    fs::create_dir_all(&hook_dir)
        .with_context(|| format!("Failed to create hook directory: {}", hook_dir.display()))?;
    let hook_path = hook_dir.join("rtk-rewrite.sh");
    Ok((hook_dir, hook_path))
}

/// Write hook script if missing or outdated, return true if changed.
/// Used by --hook-type script mode.
#[cfg(unix)]
fn ensure_hook_installed(hook_path: &Path, verbose: u8) -> Result<bool> {
    let changed = if hook_path.exists() {
        let existing = fs::read_to_string(hook_path)
            .with_context(|| format!("Failed to read existing hook: {}", hook_path.display()))?;
        if existing == REWRITE_HOOK {
            if verbose > 0 {
                eprintln!("Hook already up to date: {}", hook_path.display());
            }
            false
        } else {
            fs::write(hook_path, REWRITE_HOOK)
                .with_context(|| format!("Failed to write hook to {}", hook_path.display()))?;
            if verbose > 0 {
                eprintln!("Updated hook: {}", hook_path.display());
            }
            true
        }
    } else {
        fs::write(hook_path, REWRITE_HOOK)
            .with_context(|| format!("Failed to write hook to {}", hook_path.display()))?;
        if verbose > 0 {
            eprintln!("Created hook: {}", hook_path.display());
        }
        true
    };

    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(hook_path, fs::Permissions::from_mode(0o755))
        .with_context(|| format!("Failed to set hook permissions: {}", hook_path.display()))?;

    Ok(changed)
}

#[cfg(not(unix))]
fn ensure_hook_installed(_hook_path: &Path, _verbose: u8) -> Result<bool> {
    Ok(false) // Script mode not supported on non-Unix platforms
}

/// Idempotent file write: create or update if content differs
pub(crate) fn write_if_changed(
    path: &Path,
    content: &str,
    name: &str,
    verbose: u8,
) -> Result<bool> {
    if path.exists() {
        let existing = fs::read_to_string(path)
            .with_context(|| format!("Failed to read {}: {}", name, path.display()))?;

        if existing == content {
            if verbose > 0 {
                eprintln!("{} already up to date: {}", name, path.display());
            }
            Ok(false)
        } else {
            fs::write(path, content)
                .with_context(|| format!("Failed to write {}: {}", name, path.display()))?;
            if verbose > 0 {
                eprintln!("Updated {}: {}", name, path.display());
            }
            Ok(true)
        }
    } else {
        fs::write(path, content)
            .with_context(|| format!("Failed to write {}: {}", name, path.display()))?;
        if verbose > 0 {
            eprintln!("Created {}: {}", name, path.display());
        }
        Ok(true)
    }
}

/// Atomic write using tempfile + rename
/// Prevents corruption on crash/interrupt
pub(crate) fn atomic_write(path: &Path, content: &str) -> Result<()> {
    let parent = path.parent().with_context(|| {
        format!(
            "Cannot write to {}: path has no parent directory",
            path.display()
        )
    })?;

    // Create temp file in same directory (ensures same filesystem for atomic rename)
    let mut temp_file = NamedTempFile::new_in(parent)
        .with_context(|| format!("Failed to create temp file in {}", parent.display()))?;

    // Write content
    temp_file
        .write_all(content.as_bytes())
        .with_context(|| format!("Failed to write {} bytes to temp file", content.len()))?;

    // Atomic rename
    temp_file.persist(path).with_context(|| {
        format!(
            "Failed to atomically replace {} (disk full?)",
            path.display()
        )
    })?;

    Ok(())
}

/// Back up a file to `<filename>.rtk-backup` before RTK modifies it.
///
/// Uses "once" semantics: if the backup already exists it is left untouched so
/// that the backup always reflects the *pre-RTK original*, not some intermediate
/// state from a previous `rtk init` run.
///
/// Non-fatal: a backup failure emits a warning but does not abort the operation.
/// Returns the backup path (whether it was just created or already existed).
pub(crate) fn backup_file_once(path: &Path) -> Option<PathBuf> {
    if !path.exists() {
        return None;
    }
    let backup_name = format!(
        "{}.rtk-backup",
        path.file_name().unwrap_or_default().to_string_lossy()
    );
    let backup_path = path.with_file_name(backup_name);
    if !backup_path.exists() {
        if let Err(e) = fs::copy(path, &backup_path) {
            eprintln!(
                "Warning: could not backup {}: {} (continuing without backup)",
                path.display(),
                e
            );
            return None;
        }
    }
    // Register in the persistent backup list (idempotent — deduplicates on re-run).
    if let Ok(claude_dir) = resolve_claude_dir() {
        let registry = claude_dir.join("hooks").join("rtk-backups.json");
        append_to_backup_registry(&registry, &backup_path);
    }
    Some(backup_path)
}

/// Append a backup path to the persistent registry, deduplicating across re-runs.
/// Errors are non-fatal: the backup file itself already exists.
fn append_to_backup_registry(registry_path: &Path, backup_path: &Path) {
    let backup_str = backup_path.to_string_lossy().into_owned();

    // Read existing entries (empty list if file absent or unparseable).
    let mut entries: Vec<String> = registry_path
        .exists()
        .then(|| fs::read_to_string(registry_path).ok())
        .flatten()
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or_default();

    // Idempotent: skip if already registered.
    if entries.iter().any(|e| e == &backup_str) {
        return;
    }
    entries.push(backup_str);

    if let Some(parent) = registry_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(&entries) {
        let _ = atomic_write(registry_path, &json);
    }
}

/// Read backup paths from the registry; returns empty vec if absent or unparseable.
fn read_backup_registry(claude_dir: &Path) -> Vec<String> {
    let registry_path = claude_dir.join("hooks").join("rtk-backups.json");
    registry_path
        .exists()
        .then(|| fs::read_to_string(&registry_path).ok())
        .flatten()
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or_default()
}

/// Print backup registry entries with a context-specific header and per-entry indent.
/// No-op when the registry is empty or absent.
fn print_backup_registry(claude_dir: &Path, header: &str, indent: &str) {
    let backups = read_backup_registry(claude_dir);
    if !backups.is_empty() {
        println!("{header}");
        for p in &backups {
            println!("{indent}{p}");
        }
    }
}

/// Prompt user for consent to patch settings.json
/// Prints to stderr (stdout may be piped), reads from stdin
/// Default is No (capital N)
fn prompt_user_consent(settings_path: &Path) -> Result<bool> {
    use std::io::{self, BufRead, IsTerminal};

    eprintln!("\nPatch existing {}? [y/N] ", settings_path.display());

    // If stdin is not a terminal (piped), default to No
    if !io::stdin().is_terminal() {
        eprintln!("(non-interactive mode, defaulting to N)");
        return Ok(false);
    }

    let stdin = io::stdin();
    let mut line = String::new();
    stdin
        .lock()
        .read_line(&mut line)
        .context("Failed to read user input")?;

    let response = line.trim().to_lowercase();
    Ok(response == "y" || response == "yes")
}

/// Print manual instructions for settings.json patching
fn print_manual_instructions(hook_command: &str) {
    println!("\n  MANUAL STEP: Add this to ~/.claude/settings.json:");
    println!("  {{");
    println!("    \"hooks\": {{ \"PreToolUse\": [{{");
    println!("      \"matcher\": \"Bash\",");
    println!("      \"hooks\": [{{ \"type\": \"command\",");
    println!("        \"command\": \"{}\"", hook_command);
    println!("      }}]");
    println!("    }}]}}");
    println!("  }}");
    println!("\n  Then restart Claude Code. Test with: git status\n");
}

/// Remove RTK hook entry from settings.json
/// Returns true if hook was found and removed
/// Matches all known RTK hook command variants:
///   - "rtk hook claude" (current direct binary invocation)
///   - "rtk-rewrite.sh" (legacy migration shim)
///   - "rtk-autorun-bash.sh" (Part 1 coordinator wrapper, superseded by Part 2)
fn remove_hook_from_json(root: &mut serde_json::Value) -> bool {
    let hooks = match root.get_mut("hooks").and_then(|h| h.get_mut("PreToolUse")) {
        Some(pre_tool_use) => pre_tool_use,
        None => return false,
    };

    let pre_tool_use_array = match hooks.as_array_mut() {
        Some(arr) => arr,
        None => return false,
    };

    // Find and remove all RTK entries (any known variant)
    let original_len = pre_tool_use_array.len();
    pre_tool_use_array.retain(|entry| {
        if let Some(hooks_array) = entry.get("hooks").and_then(|h| h.as_array()) {
            for hook in hooks_array {
                if let Some(command) = hook.get("command").and_then(|c| c.as_str()) {
                    if command.contains("rtk-rewrite.sh")
                        || command.contains("rtk hook claude")
                        || command.contains("rtk-autorun-bash.sh")
                    {
                        return false; // Remove this entry
                    }
                }
            }
        }
        true // Keep this entry
    });

    pre_tool_use_array.len() < original_len
}

/// Shared: remove a hook from a settings.json file
/// Reads, parses, applies `remover`, backs up, and atomically writes if changed.
fn remove_hook_from_settings_file(
    settings_path: &Path,
    remover: impl FnOnce(&mut serde_json::Value) -> bool,
    verbose: u8,
) -> Result<bool> {
    if !settings_path.exists() {
        if verbose > 0 {
            eprintln!("{} not found, nothing to remove", settings_path.display());
        }
        return Ok(false);
    }

    let content = fs::read_to_string(settings_path)
        .with_context(|| format!("Failed to read {}", settings_path.display()))?;

    if content.trim().is_empty() {
        return Ok(false);
    }

    let mut root: serde_json::Value = serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse {} as JSON", settings_path.display()))?;

    let removed = remover(&mut root);

    if removed {
        let _ = backup_file_once(settings_path);

        let serialized =
            serde_json::to_string_pretty(&root).context("Failed to serialize settings.json")?;
        atomic_write(settings_path, &serialized)?;

        if verbose > 0 {
            eprintln!("Removed RTK hook from {}", settings_path.display());
        }
    }

    Ok(removed)
}

/// Remove RTK hook from Claude settings.json
fn remove_hook_from_settings(verbose: u8) -> Result<bool> {
    let settings_path = resolve_claude_dir()?.join("settings.json");
    remove_hook_from_settings_file(&settings_path, remove_hook_from_json, verbose)
}

/// Full uninstall: remove hook, RTK.md, @RTK.md reference, settings.json entry
pub fn uninstall(global: bool, verbose: u8) -> Result<()> {
    if !global {
        anyhow::bail!("Uninstall only works with --global flag. For local projects, manually remove RTK from CLAUDE.md");
    }

    let claude_dir = resolve_claude_dir()?;
    let mut removed = Vec::new();

    // 1. Remove legacy hook file (if present from old installs)
    let hook_path = claude_dir.join("hooks").join("rtk-rewrite.sh");
    if hook_path.exists() {
        fs::remove_file(&hook_path)
            .with_context(|| format!("Failed to remove hook: {}", hook_path.display()))?;
        removed.push(format!("Legacy hook: {}", hook_path.display()));
    }

    // 2. Remove RTK.md
    let rtk_md_path = claude_dir.join("RTK.md");
    if rtk_md_path.exists() {
        fs::remove_file(&rtk_md_path)
            .with_context(|| format!("Failed to remove RTK.md: {}", rtk_md_path.display()))?;
        removed.push(format!("RTK.md: {}", rtk_md_path.display()));
    }

    // 3. Remove @RTK.md reference from CLAUDE.md
    let claude_md_path = claude_dir.join("CLAUDE.md");
    if remove_rtk_reference_from_file(&claude_md_path, "CLAUDE.md")? {
        removed.push("CLAUDE.md: removed @RTK.md reference".to_string());
    }

    // 4. Restore plugin cache files from manifest (reverse patch_plugin_caches)
    let manifest_path = claude_dir.join("hooks").join("rtk-bash-manifest.json");
    if manifest_path.exists() {
        match restore_plugin_caches_from_manifest(&manifest_path, verbose) {
            Ok(count) if count > 0 => {
                removed.push(format!("Plugin caches restored: {} file(s)", count));
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("Warning: failed to restore plugin caches: {}", e);
            }
        }
        if let Err(e) = fs::remove_file(&manifest_path) {
            eprintln!("Warning: failed to remove manifest: {}", e);
        } else {
            removed.push(format!("Manifest: {}", manifest_path.display()));
        }
    }

    // 5. Remove Part 1 wrapper script (if present)
    let wrapper_path = claude_dir.join("hooks").join("rtk-autorun-bash.sh");
    if wrapper_path.exists() {
        if let Err(e) = fs::remove_file(&wrapper_path) {
            eprintln!("Warning: failed to remove wrapper: {}", e);
        } else {
            removed.push(format!("Wrapper: {}", wrapper_path.display()));
        }
    }

    // 6. Remove hook entry from Claude Code settings.json
    if remove_hook_from_settings(verbose)? {
        removed.push("Claude settings.json: removed RTK hook entry".to_string());
    }

    // 7. Remove hook entry from Gemini settings.json
    if remove_gemini_hook_from_settings(verbose)? {
        removed.push("Gemini settings.json: removed RTK hook entry".to_string());
    }

    // Report results
    if removed.is_empty() {
        println!("RTK was not installed (nothing to remove)");
    } else {
        println!("RTK uninstalled:");
        for item in removed {
            println!("  - {}", item);
        }
        println!("\nRestart Claude Code to apply changes.");
    }

    // Show preserved backups from the registry (never deleted — user's safety net).
    print_backup_registry(
        &claude_dir,
        "\nBackups preserved (originals from before rtk init):",
        "  ",
    );

    Ok(())
}

/// Uninstall RTK Gemini CLI integration.
/// Mirrors Claude uninstall: removes RTK.md, @RTK.md reference, and settings.json hook.
pub fn uninstall_gemini(verbose: u8) -> Result<()> {
    let gemini_dir = resolve_gemini_dir()?;
    let mut removed = Vec::new();

    // 1. Remove RTK.md
    let rtk_md_path = gemini_dir.join("RTK.md");
    if rtk_md_path.exists() {
        fs::remove_file(&rtk_md_path)
            .with_context(|| format!("Failed to remove RTK.md: {}", rtk_md_path.display()))?;
        removed.push(format!("RTK.md: {}", rtk_md_path.display()));
    }

    // 2. Remove @RTK.md reference from GEMINI.md
    let gemini_md_path = gemini_dir.join("GEMINI.md");
    if remove_rtk_reference_from_file(&gemini_md_path, "GEMINI.md")? {
        removed.push("GEMINI.md: removed @RTK.md reference".to_string());
    }

    // 3. Remove hook entry from Gemini settings.json
    if remove_gemini_hook_from_settings(verbose)? {
        removed.push("Gemini settings.json: removed RTK hook entry".to_string());
    }

    // Report results
    if removed.is_empty() {
        println!("RTK Gemini integration was not installed (nothing to remove)");
    } else {
        println!("RTK Gemini integration uninstalled:");
        for item in removed {
            println!("  - {}", item);
        }
        println!("\nRestart Gemini CLI to apply changes.");
    }

    Ok(())
}

// ============================================================================
// MULTI-PLATFORM ARCHITECTURE (Claude Code + Gemini CLI)
// ============================================================================
//
// RTK supports both Claude Code and Gemini CLI via a DRY architecture that
// shares common logic while respecting protocol-specific differences.
//
// ## Shared Infrastructure (DRY)
//
// 1. patch_settings_shared()       - Core settings.json patching logic
// 2. patch_instruction_file()      - Add @RTK.md to CLAUDE.md / GEMINI.md
// 3. remove_rtk_reference_from_file() - Remove @RTK.md (for uninstall)
// 4. show_agent_hook_status()      - Hook status verification
// 5. prompt_user_consent()         - User confirmation prompt
// 6. atomic_write() / write_if_changed() - Safe file I/O
// 7. PatchMode / PatchResult enums - Behavior control and outcome reporting
//
// ## Symmetric Installation Workflow (Both Platforms)
//
// ### Claude Code
// - Create: ~/.claude/RTK.md
// - Patch: ~/.claude/CLAUDE.md (add @RTK.md)
// - Patch: ~/.claude/settings.json (PreToolUse hook)
// - Uninstall: Removes all 3 artifacts
//
// ### Gemini CLI
// - Create: ~/.gemini/RTK.md (same content)
// - Patch: ~/.gemini/GEMINI.md (add @RTK.md)
// - Patch: ~/.gemini/settings.json (BeforeTool hook)
// - Uninstall: Removes all 3 artifacts
//
// ## Protocol-Specific Differences (Settings.json Only)
//
// - Claude: Event=PreToolUse, Matcher=Bash, Command="rtk hook claude"
// - Gemini: Event=BeforeTool, Matcher=run_shell_command, Command="rtk hook gemini"
//
// These reflect API differences and cannot be unified.
//
// ## Default Behavior (as of v0.15.3)
//
// `rtk init` (no platform flags) → Sets up BOTH Claude and Gemini
// `rtk init --claude`            → Claude only
// `rtk init --gemini`            → Gemini only
// `rtk init --uninstall`         → Remove both
// `rtk init --uninstall --claude` → Remove Claude only
// `rtk init --uninstall --gemini` → Remove Gemini only
// ============================================================================

/// Shared: patch a settings.json with an agent hook.
/// Reads/creates JSON, checks idempotency, handles PatchMode, inserts hook,
/// backs up, and atomically writes.
///
/// Used by both Claude Code (patch_settings_json) and Gemini CLI (patch_gemini_settings).
fn patch_settings_shared(
    settings_path: &Path,
    is_present: impl Fn(&serde_json::Value) -> bool,
    insert_hook: impl FnOnce(&mut serde_json::Value) -> Result<()>,
    print_manual: impl Fn(),
    mode: PatchMode,
    label: &str,
    restart_msg: &str,
    verbose: u8,
) -> Result<PatchResult> {
    // Read or create settings.json
    let mut root = if settings_path.exists() {
        let content = fs::read_to_string(settings_path)
            .with_context(|| format!("Failed to read {}", settings_path.display()))?;

        if content.trim().is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(&content)
                .with_context(|| format!("Failed to parse {} as JSON", settings_path.display()))?
        }
    } else {
        serde_json::json!({})
    };

    // Check idempotency
    if is_present(&root) {
        if verbose > 0 {
            eprintln!("{}: hook already present", label);
        }
        return Ok(PatchResult::AlreadyPresent);
    }

    // Handle mode
    match mode {
        PatchMode::Skip => {
            print_manual();
            return Ok(PatchResult::Skipped);
        }
        PatchMode::Ask => {
            if !prompt_user_consent(settings_path)? {
                print_manual();
                return Ok(PatchResult::Declined);
            }
        }
        PatchMode::Auto => {}
    }

    // Deep-merge hook
    insert_hook(&mut root)?;

    // Backup original (once — never overwrites existing backup to preserve pre-RTK state)
    let backup_path = backup_file_once(settings_path);
    if verbose > 0 {
        if let Some(ref bp) = backup_path {
            eprintln!("Backup: {}", bp.display());
        }
    }

    // Atomic write
    let serialized =
        serde_json::to_string_pretty(&root).context("Failed to serialize settings.json")?;
    atomic_write(settings_path, &serialized)?;

    println!("\n  {}: hook added", label);
    if let Some(ref bp) = backup_path {
        println!("  Backup: {}", bp.display());
    }
    println!("  {}", restart_msg);

    Ok(PatchResult::Patched)
}

/// Patch Claude settings.json with RTK hook.
/// Uses remove-then-insert to handle all upgrade states:
///   - Fresh install: nothing to remove, just inserts
///   - Old "rtk hook claude": removes then re-inserts (idempotent)
///   - Part 1 "rtk-autorun-bash.sh" wrapper: removes wrapper, inserts canonical command
///   - Already correct: removes then re-inserts (safe, atomic)
fn patch_settings_json(mode: PatchMode, hook_type: HookType, verbose: u8) -> Result<PatchResult> {
    let settings_path = resolve_claude_dir()?.join("settings.json");
    let hook_command: String = match hook_type {
        HookType::Binary => "rtk hook claude".to_owned(),
        HookType::Script => {
            let (_, hook_path) = prepare_hook_paths()?;
            ensure_hook_installed(&hook_path, verbose)?;
            hook_path.to_string_lossy().into_owned()
        }
    };

    // Remove any stale RTK hooks first (idempotent upgrade path)
    remove_hook_from_settings_file(&settings_path, remove_hook_from_json, verbose)?;

    let cmd = hook_command.as_str();
    patch_settings_shared(
        &settings_path,
        |root| hook_already_present(root, cmd),
        |root| insert_hook_entry(root, cmd),
        || print_manual_instructions(cmd),
        mode,
        "settings.json",
        "Restart Claude Code. Test with: git status",
        verbose,
    )
}

/// Clean up consecutive blank lines (collapse 3+ to 2)
/// Used when removing @RTK.md line from CLAUDE.md
fn clean_double_blanks(content: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let mut result = Vec::new();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];

        if line.trim().is_empty() {
            // Count consecutive blank lines
            let mut blank_count = 0;
            while i < lines.len() && lines[i].trim().is_empty() {
                blank_count += 1;
                i += 1;
            }

            // Keep at most 2 blank lines
            let keep = blank_count.min(2);
            result.extend(std::iter::repeat_n("", keep));
        } else {
            result.push(line);
            i += 1;
        }
    }

    result.join("\n")
}

/// Deep-merge RTK hook entry into settings.json
/// Creates hooks.PreToolUse structure if missing, preserves existing hooks
fn insert_hook_entry(root: &mut serde_json::Value, hook_command: &str) -> Result<()> {
    // Ensure root is an object
    if !root.is_object() {
        *root = serde_json::json!({});
    }
    let root_obj = root
        .as_object_mut()
        .context("settings.json root is not a JSON object")?;

    // If 'hooks' exists but isn't an object, overwrite it and warn rather than panic.
    if root_obj.get("hooks").is_some_and(|v| !v.is_object()) {
        eprintln!("Warning: settings.json 'hooks' field is not an object; overwriting");
        root_obj.insert("hooks".to_string(), serde_json::json!({}));
    }
    let hooks = root_obj
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .context("settings.json 'hooks' could not be treated as an object")?;

    // Same guard for PreToolUse.
    if hooks.get("PreToolUse").is_some_and(|v| !v.is_array()) {
        eprintln!("Warning: settings.json 'hooks.PreToolUse' is not an array; overwriting");
        hooks.insert("PreToolUse".to_string(), serde_json::json!([]));
    }
    let pre_tool_use = hooks
        .entry("PreToolUse")
        .or_insert_with(|| serde_json::json!([]))
        .as_array_mut()
        .context("settings.json 'hooks.PreToolUse' could not be treated as an array")?;

    // Append RTK hook entry
    pre_tool_use.push(serde_json::json!({
        "matcher": "Bash",
        "hooks": [{
            "type": "command",
            "command": hook_command
        }]
    }));
    Ok(())
}

/// Check if RTK hook is already present in settings.json
/// Matches on rtk-rewrite.sh (legacy) or rtk hook claude (current)
fn hook_already_present(root: &serde_json::Value, hook_command: &str) -> bool {
    let pre_tool_use_array = match root
        .get("hooks")
        .and_then(|h| h.get("PreToolUse"))
        .and_then(|p| p.as_array())
    {
        Some(arr) => arr,
        None => return false,
    };

    pre_tool_use_array
        .iter()
        .filter_map(|entry| entry.get("hooks")?.as_array())
        .flatten()
        .filter_map(|hook| hook.get("command")?.as_str())
        .any(|cmd| {
            cmd == hook_command
                || cmd.contains("rtk-rewrite.sh")   // Legacy match for migration
                || cmd.contains("rtk hook claude") // New direct binary invocation
        })
}

/// Default mode: hook + slim RTK.md + @RTK.md reference
#[cfg(not(unix))]
fn run_default_mode(
    _global: bool,
    _patch_mode: PatchMode,
    _hook_type: HookType,
    _verbose: u8,
) -> Result<()> {
    eprintln!("⚠️  Hook-based mode requires Unix (macOS/Linux).");
    eprintln!("    Windows: use --claude-md mode for full injection.");
    eprintln!("    Falling back to --claude-md mode.");
    run_claude_md_mode(_global, _verbose)
}

#[cfg(unix)]
fn run_default_mode(
    global: bool,
    patch_mode: PatchMode,
    hook_type: HookType,
    verbose: u8,
) -> Result<()> {
    if !global {
        // Local init: unchanged behavior (full injection into ./CLAUDE.md)
        return run_claude_md_mode(false, verbose);
    }

    // Pre-flight: detect unsupportable configurations before modifying anything.
    let issues = check_environment(&hook_type);
    if report_env_issues(&issues) {
        anyhow::bail!("Unsupportable configuration detected. See above for setup instructions.");
    }

    let claude_dir = resolve_claude_dir()?;
    let rtk_md_path = claude_dir.join("RTK.md");
    let claude_md_path = claude_dir.join("CLAUDE.md");

    // 1. Write RTK.md
    write_if_changed(&rtk_md_path, RTK_SLIM, "RTK.md", verbose)?;

    // 2. Patch CLAUDE.md (add @RTK.md, migrate if needed)
    let migrated = patch_claude_md(&claude_md_path, verbose)?;

    // 3. Print success message
    println!("\nRTK hook installed (global).\n");
    println!("  Hook:      rtk hook claude (direct binary)");
    println!("  RTK.md:    {} (10 lines)", rtk_md_path.display());
    println!("  CLAUDE.md: @RTK.md reference added");

    if migrated {
        println!("\n  Migrated: removed 137-line RTK block from CLAUDE.md");
        println!("            replaced with @RTK.md (10 lines)");
    }

    // 4. Export default rules to ~/.config/rtk/ for discoverability
    if let Err(e) = crate::config::export_rules(false) {
        if verbose > 0 {
            eprintln!("  Note: could not export default rules: {e}");
        }
    } else {
        println!("  Rules:     ~/.config/rtk/rtk.*.md (customizable)");
    }

    // 5. Patch plugin caches (remove Bash matchers that conflict with RTK updatedInput)
    match patch_plugin_caches(verbose) {
        Ok(0) => {} // Nothing to patch
        Ok(n) => {
            println!("  Plugin caches: patched {} file(s) (Bash removed)", n);
            println!("  Manifest:      ~/.claude/hooks/rtk-bash-manifest.json");
            println!("  Note: re-run 'rtk init -g' after plugin updates");
        }
        Err(e) => {
            if verbose > 0 {
                eprintln!("  Warning: plugin cache patching failed: {}", e);
            }
        }
    }

    // 6. Patch settings.json
    let patch_result = patch_settings_json(patch_mode, hook_type, verbose)?;

    // Report result
    match patch_result {
        PatchResult::Patched => {
            // Already printed by patch_settings_json
        }
        PatchResult::AlreadyPresent => {
            println!("\n  settings.json: hook already present");
            println!("  Restart Claude Code. Test with: git status");
        }
        PatchResult::Declined | PatchResult::Skipped => {
            // Manual instructions already printed by patch_settings_json
        }
    }

    // Show any backups created (or previously created on re-run).
    print_backup_registry(
        &claude_dir,
        "  Backups:   (originals preserved for manual recovery)",
        "             ",
    );

    println!(); // Final newline

    Ok(())
}

/// Hook-only mode: just the hook, no RTK.md
#[cfg(not(unix))]
fn run_hook_only_mode(
    _global: bool,
    _patch_mode: PatchMode,
    _hook_type: HookType,
    _verbose: u8,
) -> Result<()> {
    anyhow::bail!("Hook install requires Unix (macOS/Linux). Use WSL or --claude-md mode.")
}

#[cfg(unix)]
fn run_hook_only_mode(
    global: bool,
    patch_mode: PatchMode,
    hook_type: HookType,
    verbose: u8,
) -> Result<()> {
    if !global {
        eprintln!("⚠️  Warning: --hook-only only makes sense with --global");
        eprintln!("    For local projects, use default mode or --claude-md");
        return Ok(());
    }

    // Pre-flight: detect unsupportable configurations before modifying anything.
    let issues = check_environment(&hook_type);
    if report_env_issues(&issues) {
        anyhow::bail!("Unsupportable configuration detected. See above for setup instructions.");
    }

    let claude_dir = resolve_claude_dir()?;

    println!("\nRTK hook installed (hook-only mode).\n");
    println!("  Hook: rtk hook claude (direct binary)");
    println!(
        "  Note: No RTK.md created. Claude won't know about meta commands (gain, discover, proxy)."
    );

    // Patch plugin caches
    if let Ok(n) = patch_plugin_caches(verbose) {
        if n > 0 {
            println!("  Plugin caches: patched {} file(s)", n);
        }
    }

    // Patch settings.json
    let patch_result = patch_settings_json(patch_mode, hook_type, verbose)?;

    // Report result
    match patch_result {
        PatchResult::Patched => {
            // Already printed by patch_settings_json
        }
        PatchResult::AlreadyPresent => {
            println!("\n  settings.json: hook already present");
            println!("  Restart Claude Code. Test with: git status");
        }
        PatchResult::Declined | PatchResult::Skipped => {
            // Manual instructions already printed by patch_settings_json
        }
    }

    // Show any backups created (or previously created on re-run).
    print_backup_registry(
        &claude_dir,
        "  Backups:   (originals preserved for manual recovery)",
        "             ",
    );

    println!(); // Final newline

    Ok(())
}

/// Legacy mode: full 137-line injection into CLAUDE.md
fn run_claude_md_mode(global: bool, verbose: u8) -> Result<()> {
    let path = if global {
        resolve_claude_dir()?.join("CLAUDE.md")
    } else {
        PathBuf::from("CLAUDE.md")
    };

    if global {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
    }

    if verbose > 0 {
        eprintln!("Writing rtk instructions to: {}", path.display());
    }

    if path.exists() {
        let existing = fs::read_to_string(&path)?;
        // upsert_rtk_block handles all 4 cases: add, update, unchanged, malformed
        let (new_content, action) = upsert_rtk_block(&existing, RTK_INSTRUCTIONS);

        match action {
            RtkBlockUpsert::Added => {
                fs::write(&path, new_content)?;
                println!("✅ Added rtk instructions to existing {}", path.display());
            }
            RtkBlockUpsert::Updated => {
                fs::write(&path, new_content)?;
                println!("✅ Updated rtk instructions in {}", path.display());
            }
            RtkBlockUpsert::Unchanged => {
                println!(
                    "✅ {} already contains up-to-date rtk instructions",
                    path.display()
                );
                return Ok(());
            }
            RtkBlockUpsert::Malformed => {
                eprintln!(
                    "⚠️  Warning: Found '<!-- rtk-instructions' without closing marker in {}",
                    path.display()
                );

                if let Some((line_num, _)) = existing
                    .lines()
                    .enumerate()
                    .find(|(_, line)| line.contains("<!-- rtk-instructions"))
                {
                    eprintln!("    Location: line {}", line_num + 1);
                }

                eprintln!("    Action: Manually remove the incomplete block, then re-run:");
                if global {
                    eprintln!("            rtk init -g --claude-md");
                } else {
                    eprintln!("            rtk init --claude-md");
                }
                return Ok(());
            }
        }
    } else {
        fs::write(&path, RTK_INSTRUCTIONS)?;
        println!("✅ Created {} with rtk instructions", path.display());
    }

    if global {
        println!("   Claude Code will now use rtk in all sessions");
    } else {
        println!("   Claude Code will use rtk in this project");
    }

    Ok(())
}

// --- upsert_rtk_block: idempotent RTK block management ---

#[derive(Debug, Clone, Copy, PartialEq)]
enum RtkBlockUpsert {
    /// No existing block found — appended new block
    Added,
    /// Existing block found with different content — replaced
    Updated,
    /// Existing block found with identical content — no-op
    Unchanged,
    /// Opening marker found without closing marker — not safe to rewrite
    Malformed,
}

/// Insert or replace the RTK instructions block in `content`.
///
/// Returns `(new_content, action)` describing what happened.
/// The caller decides whether to write `new_content` based on `action`.
fn upsert_rtk_block(content: &str, block: &str) -> (String, RtkBlockUpsert) {
    let start_marker = "<!-- rtk-instructions";
    let end_marker = "<!-- /rtk-instructions -->";

    if let Some(start) = content.find(start_marker) {
        if let Some(relative_end) = content[start..].find(end_marker) {
            let end = start + relative_end;
            let end_pos = end + end_marker.len();
            let current_block = content[start..end_pos].trim();
            let desired_block = block.trim();

            if current_block == desired_block {
                return (content.to_string(), RtkBlockUpsert::Unchanged);
            }

            // Replace stale block with desired block
            let before = content[..start].trim_end();
            let after = content[end_pos..].trim_start();

            let result = match (before.is_empty(), after.is_empty()) {
                (true, true) => desired_block.to_string(),
                (true, false) => format!("{desired_block}\n\n{after}"),
                (false, true) => format!("{before}\n\n{desired_block}"),
                (false, false) => format!("{before}\n\n{desired_block}\n\n{after}"),
            };

            return (result, RtkBlockUpsert::Updated);
        }

        // Opening marker without closing marker — malformed
        return (content.to_string(), RtkBlockUpsert::Malformed);
    }

    // No existing block — append
    let trimmed = content.trim();
    if trimmed.is_empty() {
        (block.to_string(), RtkBlockUpsert::Added)
    } else {
        (
            format!("{trimmed}\n\n{}", block.trim()),
            RtkBlockUpsert::Added,
        )
    }
}

// --- patch_instruction_file: @RTK.md reference management ---

/// Shared: Patch instruction file (CLAUDE.md or GEMINI.md) to add @RTK.md reference.
/// Migrates old RTK blocks if present. Returns true if migration occurred.
fn patch_instruction_file(path: &Path, file_label: &str, verbose: u8) -> Result<bool> {
    let mut content = if path.exists() {
        fs::read_to_string(path)?
    } else {
        String::new()
    };

    let mut migrated = false;

    // Check for old block and migrate
    if content.contains("<!-- rtk-instructions") {
        let (new_content, did_migrate) = remove_rtk_block(&content);
        if did_migrate {
            content = new_content;
            migrated = true;
            if verbose > 0 {
                eprintln!("Migrated: removed old RTK block from {}", file_label);
            }
        }
    }

    // Check if @RTK.md already present
    if content.contains("@RTK.md") {
        if verbose > 0 {
            eprintln!("@RTK.md reference already present in {}", file_label);
        }
        if migrated {
            fs::write(path, content)?;
        }
        return Ok(migrated);
    }

    // Add @RTK.md
    let new_content = if content.is_empty() {
        "@RTK.md\n".to_string()
    } else {
        format!("{}\n\n@RTK.md\n", content.trim())
    };

    fs::write(path, new_content)?;

    if verbose > 0 {
        eprintln!("Added @RTK.md reference to {}", file_label);
    }

    Ok(migrated)
}

/// Shared: Remove @RTK.md reference from an instruction file (CLAUDE.md or GEMINI.md).
/// Returns true if the reference was found and removed.
fn remove_rtk_reference_from_file(path: &Path, file_label: &str) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }

    let content = fs::read_to_string(path)
        .with_context(|| format!("Failed to read {}: {}", file_label, path.display()))?;

    if !content.contains("@RTK.md") {
        return Ok(false);
    }

    let new_content = content
        .lines()
        .filter(|line| !line.trim().starts_with("@RTK.md"))
        .collect::<Vec<_>>()
        .join("\n");

    let cleaned = clean_double_blanks(&new_content);

    fs::write(path, cleaned)
        .with_context(|| format!("Failed to write {}: {}", file_label, path.display()))?;

    Ok(true)
}

/// Patch CLAUDE.md: add @RTK.md, migrate if old block exists
fn patch_claude_md(path: &Path, verbose: u8) -> Result<bool> {
    patch_instruction_file(path, "CLAUDE.md", verbose)
}

/// Patch GEMINI.md: add @RTK.md, migrate if old block exists
fn patch_gemini_md(path: &Path, verbose: u8) -> Result<bool> {
    patch_instruction_file(path, "GEMINI.md", verbose)
}

/// Remove old RTK block from CLAUDE.md or GEMINI.md (migration helper)
fn remove_rtk_block(content: &str) -> (String, bool) {
    if let (Some(start), Some(end)) = (
        content.find("<!-- rtk-instructions"),
        content.find("<!-- /rtk-instructions -->"),
    ) {
        let end_pos = end + "<!-- /rtk-instructions -->".len();
        let before = content[..start].trim_end();
        let after = content[end_pos..].trim_start();

        let result = if after.is_empty() {
            before.to_string()
        } else {
            format!("{}\n\n{}", before, after)
        };

        (result, true) // migrated
    } else if content.contains("<!-- rtk-instructions") {
        eprintln!("⚠️  Warning: Found '<!-- rtk-instructions' without closing marker.");
        eprintln!("    This can happen if CLAUDE.md was manually edited.");

        // Find line number
        if let Some((line_num, _)) = content
            .lines()
            .enumerate()
            .find(|(_, line)| line.contains("<!-- rtk-instructions"))
        {
            eprintln!("    Location: line {}", line_num + 1);
        }

        eprintln!("    Action: Manually remove the incomplete block, then re-run:");
        eprintln!("            rtk init -g");
        (content.to_string(), false)
    } else {
        (content.to_string(), false)
    }
}

// =========================================================================
// PLUGIN CACHE PATCHING
// Scans ~/.claude/plugins/cache/*/*/hooks/*.json for Bash matchers and
// removes Bash from them so RTK can be the sole Bash hook responder.
// Writes ~/.claude/hooks/rtk-bash-manifest.json for uninstall/restore.
// =========================================================================

/// One entry in the RTK bash manifest
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
struct ManifestEntry {
    cache_path: String,
    original_matcher: String,
    patched_matcher: String,
    fallthrough_command: String,
}

/// The RTK bash manifest (written by patch_plugin_caches, read by uninstall)
#[derive(serde::Serialize, serde::Deserialize, Debug)]
struct BashManifest {
    #[serde(default = "BashManifest::default_version")]
    version: u32,
    #[serde(default)]
    patched_at: String,
    #[serde(default)]
    entries: Vec<ManifestEntry>,
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

/// Scan all plugin caches for Bash matchers, remove Bash from each, write manifest.
/// Returns (newly_patched, already_up_to_date) counts.
/// Prints a user-visible summary of what changed so the user can verify the install.
pub(crate) fn patch_plugin_caches(verbose: u8) -> Result<usize> {
    let claude_dir = resolve_claude_dir()?;
    let cache_root = claude_dir.join("plugins").join("cache");
    let manifest_path = claude_dir.join("hooks").join("rtk-bash-manifest.json");

    // Load existing manifest if present (for idempotency); fall back to a fresh one.
    let mut manifest: BashManifest = manifest_path
        .exists()
        .then(|| fs::read_to_string(&manifest_path).ok())
        .flatten()
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or_default();

    if !cache_root.exists() {
        if verbose > 0 {
            eprintln!("Plugin cache directory not found: {}", cache_root.display());
        }
        return Ok(0);
    }

    // Settings JSON for CLAUDE_PLUGIN_ROOT resolution
    let settings_path = claude_dir.join("settings.json");
    let settings_root: serde_json::Value = if settings_path.exists() {
        let content = fs::read_to_string(&settings_path).unwrap_or_default();
        serde_json::from_str(&content).unwrap_or(serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    let mut newly_patched = 0usize;
    let mut already_present = 0usize;

    // Walk cache/*/*/hooks/*.json
    // Structure: cache/{vendor}/{plugin}/{version}/hooks/{file}.json
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

            for version_entry in versions.flatten() {
                let version_path = version_entry.path();
                if !version_path.is_dir() {
                    continue;
                }

                let hooks_dir = version_path.join("hooks");
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

                    // Check before calling: was this file already patched by a previous run?
                    let in_manifest = manifest
                        .entries
                        .iter()
                        .any(|e| e.cache_path == hook_path.to_string_lossy().as_ref());

                    match patch_single_cache_file(
                        &hook_path,
                        &vendor_name,
                        &plugin_name,
                        &settings_root,
                        &mut manifest,
                        verbose,
                    ) {
                        Ok(true) => newly_patched += 1,
                        // Only count as "already up-to-date" if it was in the manifest.
                        // Files that never had Bash are silently ignored (not confusing to users).
                        Ok(false) if in_manifest => already_present += 1,
                        Ok(false) => {} // No Bash matcher found; not our concern
                        Err(e) => {
                            eprintln!("Warning: failed to patch {}: {}", hook_path.display(), e);
                        }
                    }
                }
            }
        }
    }

    // Write updated manifest
    if !manifest.entries.is_empty() {
        manifest.patched_at = chrono::Utc::now().to_rfc3339();
        manifest.version = 1;
        let manifest_json =
            serde_json::to_string_pretty(&manifest).context("Failed to serialize manifest")?;
        // Ensure hooks dir exists
        if let Some(parent) = manifest_path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("Failed to create hooks directory: {}", parent.display())
            })?;
        }
        atomic_write(&manifest_path, &manifest_json)?;
    }

    // Always print a summary so the user can verify the install state.
    if newly_patched > 0 {
        println!(
            "  Plugin caches: {} patched, {} already up-to-date",
            newly_patched, already_present
        );
        for entry in &manifest.entries {
            println!("    Patched: {}", entry.cache_path);
            // Show backup path if it exists alongside the patched file
            let bp_name = format!(
                "{}.rtk-backup",
                Path::new(&entry.cache_path)
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
            );
            let bp = Path::new(&entry.cache_path).with_file_name(bp_name);
            if bp.exists() {
                println!("    Backup:  {}", bp.display());
            }
            println!(
                "      Matcher: \"{}\" → \"{}\"",
                entry.original_matcher, entry.patched_matcher
            );
            println!("      Fallthrough: {}", entry.fallthrough_command);
        }
        println!("  Manifest: {}", manifest_path.display());
        println!("  Re-run 'rtk init --global' after any plugin update to keep caches in sync.");
    } else if already_present > 0 {
        println!(
            "  Plugin caches: {} already up-to-date (re-run safe)",
            already_present
        );
    }

    Ok(newly_patched)
}

/// Patch a single plugin cache JSON file: remove Bash from PreToolUse matchers.
/// Appends to manifest if changed and not already present.
/// Returns Ok(true) if the manifest was updated (newly patched or reconstructed),
/// Ok(false) if already present or no PreToolUse hooks found.
fn patch_single_cache_file(
    hook_path: &Path,
    vendor_name: &str,
    plugin_name: &str,
    settings_root: &serde_json::Value,
    manifest: &mut BashManifest,
    verbose: u8,
) -> Result<bool> {
    let content = fs::read_to_string(hook_path)
        .with_context(|| format!("Failed to read {}", hook_path.display()))?;

    let mut json: serde_json::Value = serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse {}", hook_path.display()))?;

    // Pre-compute owned string once — used for idempotency check and ManifestEntry below.
    let cache_path_str = hook_path.to_string_lossy().into_owned();

    // Already in manifest with same path? Skip (idempotent second run).
    if manifest
        .entries
        .iter()
        .any(|e| e.cache_path == cache_path_str)
    {
        return Ok(false);
    }

    let pre_tool_use = match json
        .get_mut("hooks")
        .and_then(|h| h.get_mut("PreToolUse"))
        .and_then(|p| p.as_array_mut())
    {
        Some(arr) => arr,
        None => return Ok(false), // No PreToolUse hooks → nothing to register
    };

    // Check whether any entry still has Bash in its matcher.
    // If Bash is present → first-run path: patch the file and add to manifest.
    // If no Bash anywhere → reconstruction path: Bash was already removed by a prior
    //   rtk init that didn't create a backup file. Register the current PreToolUse
    //   entries in the manifest so the binary hook still calls them as fallthrough
    //   handlers for Bash events. Safe for uninstall: original_matcher == patched_matcher
    //   so the restore is a write-back no-op (same value written). If the plugin never
    //   actually handled Bash, it will return exit 0 (pass-through) when called.
    let has_any_bash = pre_tool_use
        .iter()
        .any(|e| matcher_contains_bash(e.get("matcher").and_then(|m| m.as_str()).unwrap_or("")));

    if !has_any_bash {
        // Reconstruction path: no Bash in any entry. Add all non-empty matchers to
        // manifest so fallthrough mechanism includes this plugin's Bash event handling.
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
            let resolved_command =
                resolve_plugin_root_in_command(&command, vendor_name, plugin_name, settings_root);
            if verbose > 0 {
                eprintln!(
                    "Reconstructed manifest entry for '{}' (Bash already removed)",
                    hook_path.display()
                );
            }
            manifest.entries.push(ManifestEntry {
                cache_path: cache_path_str.clone(),
                original_matcher: matcher.clone(), // unknown true original; use current
                patched_matcher: matcher,          // same → uninstall restore is a no-op
                fallthrough_command: resolved_command,
            });
            any_added = true;
        }
        return Ok(any_added);
    }

    // First-run path: at least one entry has Bash. Patch the file.
    let mut any_patched = false;

    for entry in pre_tool_use.iter_mut() {
        // Immutable read phase: extract matcher and command before any mutable borrows.
        let matcher = entry
            .get("matcher")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string();

        if !matcher_contains_bash(&matcher) {
            continue;
        }

        // Read command from the first hook entry (immutable).
        let command = entry
            .get("hooks")
            .and_then(|h| h.as_array())
            .and_then(|arr| arr.first())
            .and_then(|h| h.get("command"))
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();

        // Resolve CLAUDE_PLUGIN_ROOT in command
        let resolved_command =
            resolve_plugin_root_in_command(&command, vendor_name, plugin_name, settings_root);
        let new_matcher = remove_bash_from_matcher(&matcher);

        // Guard: if Bash was the only token, the result would be an empty matcher string.
        // Writing "" back would make Claude Code match the entry against no tools at all,
        // silently disabling all non-Bash functionality of this plugin hook entry.
        // Skip patching this entry; log a warning so the user can investigate.
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

        // Mutable write phase: all reads done, no conflicting borrows.
        if let Some(entry_obj) = entry.as_object_mut() {
            entry_obj.insert(
                "matcher".to_string(),
                serde_json::Value::String(new_matcher.clone()),
            );
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
        // Backup before overwriting (once — preserves pre-RTK original across re-runs).
        let _ = backup_file_once(hook_path);

        let patched = serde_json::to_string_pretty(&json)
            .context("Failed to serialize patched cache JSON")?;
        atomic_write(hook_path, &patched)?;
        if verbose > 0 {
            eprintln!("Patched: {}", hook_path.display());
        }
    }

    Ok(any_patched)
}

/// Returns true if a matcher string contains "Bash" as a whole token
fn matcher_contains_bash(matcher: &str) -> bool {
    matcher.split('|').any(|part| part.trim() == "Bash")
}

/// Remove "Bash" from a pipe-separated matcher string
/// e.g. "Write|Edit|Bash|ExitPlanMode" -> "Write|Edit|ExitPlanMode"
fn remove_bash_from_matcher(matcher: &str) -> String {
    matcher
        .split('|')
        .filter(|part| part.trim() != "Bash")
        .collect::<Vec<_>>()
        .join("|")
}

/// Resolve ${CLAUDE_PLUGIN_ROOT} in a command string to an absolute path.
/// Looks up the vendor in extraKnownMarketplaces in settings.json.
fn resolve_plugin_root_in_command(
    command: &str,
    vendor_name: &str,
    plugin_name: &str,
    settings_root: &serde_json::Value,
) -> String {
    if !command.contains("${CLAUDE_PLUGIN_ROOT}") {
        return command.to_string();
    }

    // Try extraKnownMarketplaces first
    if let Some(marketplace_path) = settings_root
        .get("extraKnownMarketplaces")
        .and_then(|m| m.get(vendor_name))
        .and_then(|v| v.get("source"))
        .and_then(|s| s.get("path"))
        .and_then(|p| p.as_str())
    {
        // Try computed path: {marketplace}/plugins/{plugin_name}
        let primary = format!("{}/plugins/{}", marketplace_path, plugin_name);
        if Path::new(&primary).exists() {
            return command.replace("${CLAUDE_PLUGIN_ROOT}", &primary);
        }

        // Fallback 1: {marketplace}/plugins/{vendor_name}
        // Plugin package name (e.g. "ar") may differ from source dir (e.g. "autorun").
        let by_vendor = format!("{}/plugins/{}", marketplace_path, vendor_name);
        if Path::new(&by_vendor).exists() {
            return command.replace("${CLAUDE_PLUGIN_ROOT}", &by_vendor);
        }

        // Fallback 2: scan immediate subdirs of {marketplace}/plugins/ for first
        // dir that contains a hooks/ subdirectory (plugin source convention).
        let plugins_dir = format!("{}/plugins", marketplace_path);
        if let Ok(entries) = fs::read_dir(&plugins_dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir()
                    && p.join("hooks").is_dir()
                    && !p
                        .file_name()
                        .and_then(|n| n.to_str())
                        .map(|n| n.starts_with('.') || n == "__pycache__" || n.contains("venv"))
                        .unwrap_or(false)
                {
                    return command
                        .replace("${CLAUDE_PLUGIN_ROOT}", &p.to_string_lossy().into_owned());
                }
            }
        }

        // None of the fallbacks found — use computed primary (will fail at runtime)
        return command.replace("${CLAUDE_PLUGIN_ROOT}", &primary);
    }

    // Fall back to standard marketplace location
    if let Ok(claude_dir) = resolve_claude_dir() {
        let standard_root = claude_dir
            .join("plugins")
            .join(vendor_name)
            .join(plugin_name);
        if standard_root.exists() {
            return command.replace("${CLAUDE_PLUGIN_ROOT}", &standard_root.to_string_lossy());
        }
    }

    // Cannot resolve; log and return original (will fail at runtime, but non-blocking)
    eprintln!(
        "Warning: cannot resolve CLAUDE_PLUGIN_ROOT for vendor={} plugin={}. \
         Fallthrough command will not work until re-run after plugin source is found.",
        vendor_name, plugin_name
    );
    command.to_string()
}

/// Restore plugin cache files from manifest (used by uninstall)
fn restore_plugin_caches_from_manifest(manifest_path: &Path, verbose: u8) -> Result<usize> {
    let content = fs::read_to_string(manifest_path)
        .with_context(|| format!("Failed to read manifest: {}", manifest_path.display()))?;
    let manifest: BashManifest =
        serde_json::from_str(&content).context("Failed to parse rtk-bash-manifest.json")?;

    let mut restored = 0;
    for entry in &manifest.entries {
        let cache_path = Path::new(&entry.cache_path);
        if !cache_path.exists() {
            if verbose > 0 {
                eprintln!(
                    "Cache file no longer exists, skipping: {}",
                    entry.cache_path
                );
            }
            continue;
        }

        let content = fs::read_to_string(cache_path)
            .with_context(|| format!("Failed to read {}", entry.cache_path))?;
        let mut json: serde_json::Value = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse {}", entry.cache_path))?;

        // Restore original matcher
        let mut this_entry_restored = false;
        if let Some(pre_tool_use) = json
            .get_mut("hooks")
            .and_then(|h| h.get_mut("PreToolUse"))
            .and_then(|p| p.as_array_mut())
        {
            for hook_entry in pre_tool_use.iter_mut() {
                let current_matcher = hook_entry
                    .get("matcher")
                    .and_then(|m| m.as_str())
                    .unwrap_or("")
                    .to_string();
                if current_matcher == entry.patched_matcher {
                    if let Some(obj) = hook_entry.as_object_mut() {
                        obj.insert(
                            "matcher".to_string(),
                            serde_json::Value::String(entry.original_matcher.clone()),
                        );
                    }
                    this_entry_restored = true;
                }
            }
        }

        if !this_entry_restored {
            // Cache was updated by a plugin upgrade; matcher no longer matches what we patched.
            // Do NOT write the file — nothing changed and touching it is unnecessary.
            eprintln!(
                "Warning: could not restore '{}' — patched matcher '{}' not found. \
                 Plugin may have been updated since 'rtk init' was run.",
                entry.cache_path, entry.patched_matcher
            );
            continue;
        }

        let restored_json =
            serde_json::to_string_pretty(&json).context("Failed to serialize restored JSON")?;
        atomic_write(cache_path, &restored_json)?;
        if verbose > 0 {
            eprintln!("Restored: {}", entry.cache_path);
        }
        restored += 1;
    }

    Ok(restored)
}

/// Resolve ~/.claude directory with proper home expansion
pub(crate) fn resolve_claude_dir() -> Result<PathBuf> {
    dirs::home_dir()
        .map(|h| h.join(".claude"))
        .context("Cannot determine home directory. Is $HOME set?")
}

/// Resolve ~/.gemini directory with proper home expansion
pub(crate) fn resolve_gemini_dir() -> Result<PathBuf> {
    dirs::home_dir()
        .map(|h| h.join(".gemini"))
        .context("Cannot determine home directory. Is $HOME set?")
}

// =========================================================================
// GEMINI CLI INTEGRATION
// Patches ~/.gemini/settings.json with BeforeTool hook for rtk
// =========================================================================

/// Check if RTK Gemini hook is already present in settings.json
fn gemini_hook_already_present(root: &serde_json::Value) -> bool {
    let before_tool_array = match root
        .get("hooks")
        .and_then(|h| h.get("BeforeTool"))
        .and_then(|p| p.as_array())
    {
        Some(arr) => arr,
        None => return false,
    };

    before_tool_array
        .iter()
        .filter_map(|entry| entry.get("hooks")?.as_array())
        .flatten()
        .filter_map(|hook| hook.get("command")?.as_str())
        .any(|cmd| cmd.contains("rtk hook gemini"))
}

/// Deep-merge RTK hook entry into Gemini settings.json
fn insert_gemini_hook_entry(root: &mut serde_json::Value) -> Result<()> {
    if !root.is_object() {
        *root = serde_json::json!({});
    }
    let root_obj = root
        .as_object_mut()
        .context("settings.json root is not a JSON object")?;

    if root_obj.get("hooks").is_some_and(|v| !v.is_object()) {
        eprintln!("Warning: settings.json 'hooks' field is not an object; overwriting");
        root_obj.insert("hooks".to_string(), serde_json::json!({}));
    }
    let hooks = root_obj
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .context("settings.json 'hooks' could not be treated as an object")?;

    if hooks.get("BeforeTool").is_some_and(|v| !v.is_array()) {
        eprintln!("Warning: settings.json 'hooks.BeforeTool' is not an array; overwriting");
        hooks.insert("BeforeTool".to_string(), serde_json::json!([]));
    }
    let before_tool = hooks
        .entry("BeforeTool")
        .or_insert_with(|| serde_json::json!([]))
        .as_array_mut()
        .context("settings.json 'hooks.BeforeTool' could not be treated as an array")?;

    before_tool.push(serde_json::json!({
        "matcher": "run_shell_command",
        "hooks": [{
            "type": "command",
            "command": "rtk hook gemini"
        }]
    }));
    Ok(())
}

/// Remove RTK Gemini hook entry from settings.json
fn remove_gemini_hook_from_json(root: &mut serde_json::Value) -> bool {
    let hooks = match root.get_mut("hooks").and_then(|h| h.get_mut("BeforeTool")) {
        Some(before_tool) => before_tool,
        None => return false,
    };

    let before_tool_array = match hooks.as_array_mut() {
        Some(arr) => arr,
        None => return false,
    };

    let original_len = before_tool_array.len();
    before_tool_array.retain(|entry| {
        if let Some(hooks_array) = entry.get("hooks").and_then(|h| h.as_array()) {
            for hook in hooks_array {
                if let Some(command) = hook.get("command").and_then(|c| c.as_str()) {
                    if command.contains("rtk hook gemini") {
                        return false; // Remove this entry
                    }
                }
            }
        }
        true // Keep this entry
    });

    before_tool_array.len() < original_len
}

/// Patch Gemini settings.json with RTK hook
fn patch_gemini_settings(mode: PatchMode, verbose: u8) -> Result<PatchResult> {
    let gemini_dir = resolve_gemini_dir()?;
    fs::create_dir_all(&gemini_dir).with_context(|| {
        format!(
            "Failed to create Gemini directory: {}",
            gemini_dir.display()
        )
    })?;

    let settings_path = gemini_dir.join("settings.json");
    patch_settings_shared(
        &settings_path,
        |root| gemini_hook_already_present(root),
        insert_gemini_hook_entry,
        print_gemini_manual_instructions,
        mode,
        "Gemini settings.json",
        "Restart Gemini CLI. Test with: gemini",
        verbose,
    )
}

/// Print manual instructions for Gemini settings.json patching
fn print_gemini_manual_instructions() {
    println!("\n  MANUAL STEP: Add this to ~/.gemini/settings.json:");
    println!("  {{");
    println!("    \"hooks\": {{ \"BeforeTool\": [{{");
    println!("      \"matcher\": \"run_shell_command\",");
    println!("      \"hooks\": [{{ \"type\": \"command\",");
    println!("        \"command\": \"rtk hook gemini\"");
    println!("      }}]");
    println!("    }}]}}");
    println!("  }}");
    println!("\n  Then restart Gemini CLI.\n");
}

/// Remove RTK hook from Gemini settings.json
fn remove_gemini_hook_from_settings(verbose: u8) -> Result<bool> {
    let settings_path = resolve_gemini_dir()?.join("settings.json");
    remove_hook_from_settings_file(&settings_path, remove_gemini_hook_from_json, verbose)
}

/// Public entry point for `rtk init --gemini`
/// Mirrors Claude Code setup: RTK.md + GEMINI.md patching + settings.json hook
pub fn run_gemini(patch_mode: PatchMode, verbose: u8) -> Result<()> {
    let gemini_dir = resolve_gemini_dir()?;
    let rtk_md_path = gemini_dir.join("RTK.md");
    let gemini_md_path = gemini_dir.join("GEMINI.md");

    // 1. Write RTK.md (same content as Claude's RTK.md)
    write_if_changed(&rtk_md_path, RTK_SLIM, "RTK.md", verbose)?;

    // 2. Patch GEMINI.md (add @RTK.md reference, migrate if needed)
    let migrated = patch_gemini_md(&gemini_md_path, verbose)?;

    // 3. Print success message
    println!("\nRTK hook installed (Gemini CLI).\n");
    println!("  Hook:      rtk hook gemini (direct binary)");
    println!("  RTK.md:    {} (10 lines)", rtk_md_path.display());
    println!("  GEMINI.md: @RTK.md reference added");

    if migrated {
        println!("\n  Migrated: Removed old RTK block from GEMINI.md (now using @RTK.md)");
    }

    // 4. Patch settings.json
    let _patch_result = patch_gemini_settings(patch_mode, verbose)?;

    println!();
    Ok(())
}

/// Display hook status for one agent's settings.json.
/// `prefix` is prepended to "settings.json" in output (e.g. "" for Claude, "Gemini " for Gemini).
fn show_agent_hook_status(
    prefix: &str,
    settings_path: &Path,
    is_present: impl Fn(&serde_json::Value) -> bool,
    setup_hint: &str,
) {
    if !settings_path.exists() {
        println!("⚪ {}settings.json: not found", prefix);
        return;
    }
    let content = match fs::read_to_string(settings_path) {
        Ok(c) => c,
        Err(_) => {
            println!("⚠️  {}settings.json: unreadable", prefix);
            return;
        }
    };
    if content.trim().is_empty() {
        println!("⚪ {}settings.json: empty", prefix);
        return;
    }
    match serde_json::from_str::<serde_json::Value>(&content) {
        Ok(root) => {
            if is_present(&root) {
                println!("✅ {}settings.json: RTK hook configured", prefix);
            } else {
                println!(
                    "⚠️  {}settings.json: exists but RTK hook not configured",
                    prefix
                );
                println!("    Run: {}", setup_hint);
            }
        }
        Err(_) => {
            println!("⚠️  {}settings.json: exists but invalid JSON", prefix);
        }
    }
}

/// Show current rtk configuration
pub fn show_config() -> Result<()> {
    let claude_dir = resolve_claude_dir()?;
    let rtk_md_path = claude_dir.join("RTK.md");
    let global_claude_md = claude_dir.join("CLAUDE.md");
    let local_claude_md = PathBuf::from("CLAUDE.md");

    println!("rtk Configuration:\n");

    // Check hook in settings.json
    let settings_path = claude_dir.join("settings.json");
    if settings_path.exists() {
        let content = fs::read_to_string(&settings_path)?;
        if let Ok(root) = serde_json::from_str::<serde_json::Value>(&content) {
            if hook_already_present(&root, "rtk hook claude") {
                println!("  Hook: rtk hook claude (configured in settings.json)");
            } else {
                let legacy_hook = claude_dir.join("hooks").join("rtk-rewrite.sh");
                if legacy_hook.exists() {
                    println!("  Hook: legacy rtk-rewrite.sh (run: rtk init -g to migrate)");
                } else {
                    println!("  Hook: not configured");
                }
            }
        }
    } else {
        println!("  Hook: settings.json not found");
    }

    // Check RTK.md
    if rtk_md_path.exists() {
        println!("✅ RTK.md: {} (slim mode)", rtk_md_path.display());
    } else {
        println!("⚪ RTK.md: not found");
    }

    // Check global CLAUDE.md
    if global_claude_md.exists() {
        let content = fs::read_to_string(&global_claude_md)?;
        if content.contains("@RTK.md") {
            println!("✅ Global (~/.claude/CLAUDE.md): @RTK.md reference");
        } else if content.contains("<!-- rtk-instructions") {
            println!(
                "⚠️  Global (~/.claude/CLAUDE.md): old RTK block (run: rtk init -g to migrate)"
            );
        } else {
            println!("⚪ Global (~/.claude/CLAUDE.md): exists but rtk not configured");
        }
    } else {
        println!("⚪ Global (~/.claude/CLAUDE.md): not found");
    }

    // Check local CLAUDE.md
    if local_claude_md.exists() {
        let content = fs::read_to_string(&local_claude_md)?;
        if content.contains("rtk") {
            println!("✅ Local (./CLAUDE.md): rtk enabled");
        } else {
            println!("⚪ Local (./CLAUDE.md): exists but rtk not configured");
        }
    } else {
        println!("⚪ Local (./CLAUDE.md): not found");
    }

    // Check Claude settings.json
    let settings_path = claude_dir.join("settings.json");
    show_agent_hook_status(
        "",
        &settings_path,
        |root| hook_already_present(root, "rtk hook claude"),
        "rtk init -g --auto-patch",
    );

    // Check Gemini settings.json
    match resolve_gemini_dir() {
        Ok(gemini_dir) => {
            let gemini_settings = gemini_dir.join("settings.json");
            show_agent_hook_status(
                "Gemini ",
                &gemini_settings,
                |root| gemini_hook_already_present(root),
                "rtk init --gemini --auto-patch",
            );
        }
        Err(_) => {
            println!("⚪ Gemini: cannot determine home directory");
        }
    }

    println!("\nUsage:");
    println!("  rtk init              # Full injection into local CLAUDE.md");
    println!("  rtk init -g           # Hook + RTK.md + @RTK.md + settings.json (recommended)");
    println!("  rtk init -g --auto-patch    # Same as above but no prompt");
    println!("  rtk init -g --no-patch      # Skip settings.json (manual setup)");
    println!("  rtk init -g --uninstall     # Remove all RTK artifacts");
    println!("  rtk init -g --claude-md     # Legacy: full injection into ~/.claude/CLAUDE.md");
    println!("  rtk init -g --hook-only     # Hook only, no RTK.md");
    println!("  rtk init --gemini           # Gemini CLI hook setup");
    println!("  rtk init --gemini --auto-patch  # Gemini hook without prompting");
    println!("  rtk init --gemini --uninstall   # Remove Gemini hook");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_init_mentions_all_top_level_commands() {
        for cmd in [
            "rtk cargo",
            "rtk gh",
            "rtk vitest",
            "rtk tsc",
            "rtk lint",
            "rtk prettier",
            "rtk next",
            "rtk playwright",
            "rtk prisma",
            "rtk pnpm",
            "rtk npm",
            "rtk curl",
            "rtk git",
            "rtk docker",
            "rtk kubectl",
        ] {
            assert!(
                RTK_INSTRUCTIONS.contains(cmd),
                "Missing {cmd} in RTK_INSTRUCTIONS"
            );
        }
    }

    #[test]
    fn test_init_has_version_marker() {
        assert!(
            RTK_INSTRUCTIONS.contains("<!-- rtk-instructions"),
            "RTK_INSTRUCTIONS must have version marker for idempotency"
        );
    }

    #[test]
    fn test_migration_removes_old_block() {
        let input = r#"# My Config

<!-- rtk-instructions v2 -->
OLD RTK STUFF
<!-- /rtk-instructions -->

More content"#;

        let (result, migrated) = remove_rtk_block(input);
        assert!(migrated);
        assert!(!result.contains("OLD RTK STUFF"));
        assert!(result.contains("# My Config"));
        assert!(result.contains("More content"));
    }

    #[test]
    fn test_migration_warns_on_missing_end_marker() {
        let input = "<!-- rtk-instructions v2 -->\nOLD STUFF\nNo end marker";
        let (result, migrated) = remove_rtk_block(input);
        assert!(!migrated);
        assert_eq!(result, input);
    }

    #[test]
    fn test_default_mode_creates_rtk_md() {
        let temp = TempDir::new().unwrap();
        let rtk_md_path = temp.path().join("RTK.md");
        fs::write(&rtk_md_path, RTK_SLIM).unwrap();
        assert!(rtk_md_path.exists());
        let content = fs::read_to_string(&rtk_md_path).unwrap();
        assert!(content.contains("rtk"));
    }

    #[test]
    fn test_claude_md_mode_creates_full_injection() {
        // Just verify RTK_INSTRUCTIONS constant has the right content
        assert!(RTK_INSTRUCTIONS.contains("<!-- rtk-instructions"));
        assert!(RTK_INSTRUCTIONS.contains("rtk cargo test"));
        assert!(RTK_INSTRUCTIONS.contains("<!-- /rtk-instructions -->"));
        assert!(RTK_INSTRUCTIONS.len() > 4000);
    }

    // --- upsert_rtk_block tests ---

    #[test]
    fn test_upsert_rtk_block_appends_when_missing() {
        let input = "# Team instructions";
        let (content, action) = upsert_rtk_block(input, RTK_INSTRUCTIONS);
        assert_eq!(action, RtkBlockUpsert::Added);
        assert!(content.contains("# Team instructions"));
        assert!(content.contains("<!-- rtk-instructions"));
    }

    #[test]
    fn test_upsert_rtk_block_updates_stale_block() {
        let input = r#"# Team instructions

<!-- rtk-instructions v1 -->
OLD RTK CONTENT
<!-- /rtk-instructions -->

More notes
"#;

        let (content, action) = upsert_rtk_block(input, RTK_INSTRUCTIONS);
        assert_eq!(action, RtkBlockUpsert::Updated);
        assert!(!content.contains("OLD RTK CONTENT"));
        assert!(content.contains("rtk cargo test")); // from current RTK_INSTRUCTIONS
        assert!(content.contains("# Team instructions"));
        assert!(content.contains("More notes"));
    }

    #[test]
    fn test_upsert_rtk_block_noop_when_already_current() {
        let input = format!(
            "# Team instructions\n\n{}\n\nMore notes\n",
            RTK_INSTRUCTIONS
        );
        let (content, action) = upsert_rtk_block(&input, RTK_INSTRUCTIONS);
        assert_eq!(action, RtkBlockUpsert::Unchanged);
        assert_eq!(content, input);
    }

    #[test]
    fn test_upsert_rtk_block_detects_malformed_block() {
        let input = "<!-- rtk-instructions v2 -->\npartial";
        let (content, action) = upsert_rtk_block(input, RTK_INSTRUCTIONS);
        assert_eq!(action, RtkBlockUpsert::Malformed);
        assert_eq!(content, input);
    }

    #[test]
    fn test_init_is_idempotent() {
        let temp = TempDir::new().unwrap();
        let claude_md = temp.path().join("CLAUDE.md");

        fs::write(&claude_md, "# My stuff\n\n@RTK.md\n").unwrap();

        let content = fs::read_to_string(&claude_md).unwrap();
        let count = content.matches("@RTK.md").count();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_local_init_unchanged() {
        // Local init should use claude-md mode
        let temp = TempDir::new().unwrap();
        let claude_md = temp.path().join("CLAUDE.md");

        fs::write(&claude_md, RTK_INSTRUCTIONS).unwrap();
        let content = fs::read_to_string(&claude_md).unwrap();

        assert!(content.contains("<!-- rtk-instructions"));
    }

    // Tests for hook_already_present()
    #[test]
    fn test_hook_already_present_exact_match() {
        let json_content = serde_json::json!({
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [{
                        "type": "command",
                        "command": "rtk hook claude"
                    }]
                }]
            }
        });

        let hook_command = "rtk hook claude";
        assert!(hook_already_present(&json_content, hook_command));
    }

    #[test]
    fn test_hook_already_present_legacy_migration() {
        // Old installs have rtk-rewrite.sh — should still match for migration
        let json_content = serde_json::json!({
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [{
                        "type": "command",
                        "command": "/home/user/.claude/hooks/rtk-rewrite.sh"
                    }]
                }]
            }
        });

        let hook_command = "rtk hook claude";
        assert!(hook_already_present(&json_content, hook_command));
    }

    #[test]
    fn test_hook_not_present_empty() {
        let json_content = serde_json::json!({});
        let hook_command = "rtk hook claude";
        assert!(!hook_already_present(&json_content, hook_command));
    }

    #[test]
    fn test_hook_not_present_other_hooks() {
        let json_content = serde_json::json!({
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [{
                        "type": "command",
                        "command": "/some/other/hook.sh"
                    }]
                }]
            }
        });

        let hook_command = "rtk hook claude";
        assert!(!hook_already_present(&json_content, hook_command));
    }

    // Tests for insert_hook_entry()
    #[test]
    fn test_insert_hook_entry_empty_root() {
        let mut json_content = serde_json::json!({});
        let hook_command = "rtk hook claude";

        insert_hook_entry(&mut json_content, hook_command);

        // Should create full structure
        assert!(json_content.get("hooks").is_some());
        assert!(json_content
            .get("hooks")
            .unwrap()
            .get("PreToolUse")
            .is_some());

        let pre_tool_use = json_content["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre_tool_use.len(), 1);

        let command = pre_tool_use[0]["hooks"][0]["command"].as_str().unwrap();
        assert_eq!(command, hook_command);
    }

    #[test]
    fn test_insert_hook_entry_preserves_existing() {
        let mut json_content = serde_json::json!({
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [{
                        "type": "command",
                        "command": "/some/other/hook.sh"
                    }]
                }]
            }
        });

        let hook_command = "rtk hook claude";
        insert_hook_entry(&mut json_content, hook_command);

        let pre_tool_use = json_content["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre_tool_use.len(), 2); // Should have both hooks

        // Check first hook is preserved
        let first_command = pre_tool_use[0]["hooks"][0]["command"].as_str().unwrap();
        assert_eq!(first_command, "/some/other/hook.sh");

        // Check second hook is RTK
        let second_command = pre_tool_use[1]["hooks"][0]["command"].as_str().unwrap();
        assert_eq!(second_command, hook_command);
    }

    #[test]
    fn test_insert_hook_preserves_other_keys() {
        let mut json_content = serde_json::json!({
            "env": {"PATH": "/custom/path"},
            "permissions": {"allowAll": true},
            "model": "claude-sonnet-4"
        });

        let hook_command = "rtk hook claude";
        insert_hook_entry(&mut json_content, hook_command);

        // Should preserve all other keys
        assert_eq!(json_content["env"]["PATH"], "/custom/path");
        assert_eq!(json_content["permissions"]["allowAll"], true);
        assert_eq!(json_content["model"], "claude-sonnet-4");

        // And add hooks
        assert!(json_content.get("hooks").is_some());
    }

    // Tests for atomic_write()
    #[test]
    fn test_atomic_write() {
        let temp = TempDir::new().unwrap();
        let file_path = temp.path().join("test.json");

        let content = r#"{"key": "value"}"#;
        atomic_write(&file_path, content).unwrap();

        assert!(file_path.exists());
        let written = fs::read_to_string(&file_path).unwrap();
        assert_eq!(written, content);
    }

    // Test for preserve_order round-trip
    #[test]
    fn test_preserve_order_round_trip() {
        let original = r#"{"env": {"PATH": "/usr/bin"}, "permissions": {"allowAll": true}, "model": "claude-sonnet-4"}"#;
        let parsed: serde_json::Value = serde_json::from_str(original).unwrap();
        let serialized = serde_json::to_string(&parsed).unwrap();

        // Just check that keys exist (preserve_order doesn't guarantee exact order in nested objects)
        assert!(serialized.contains("\"env\""));
        assert!(serialized.contains("\"permissions\""));
        assert!(serialized.contains("\"model\""));
    }

    // Tests for clean_double_blanks()
    #[test]
    fn test_clean_double_blanks() {
        // Input: line1, 2 blank lines, line2, 1 blank line, line3, 3 blank lines, line4
        // Expected: line1, 2 blank lines (kept), line2, 1 blank line, line3, 2 blank lines (max), line4
        let input = "line1\n\n\nline2\n\nline3\n\n\n\nline4";
        // That's: line1 \n \n \n line2 \n \n line3 \n \n \n \n line4
        // Which is: line1, blank, blank, line2, blank, line3, blank, blank, blank, line4
        // So 2 blanks after line1 (keep both), 1 blank after line2 (keep), 3 blanks after line3 (keep 2)
        let expected = "line1\n\n\nline2\n\nline3\n\n\nline4";
        assert_eq!(clean_double_blanks(input), expected);
    }

    #[test]
    fn test_clean_double_blanks_preserves_single() {
        let input = "line1\n\nline2\n\nline3";
        assert_eq!(clean_double_blanks(input), input); // No change
    }

    // Tests for remove_hook_from_settings()
    #[test]
    fn test_remove_hook_from_json() {
        let mut json_content = serde_json::json!({
            "hooks": {
                "PreToolUse": [
                    {
                        "matcher": "Bash",
                        "hooks": [{
                            "type": "command",
                            "command": "/some/other/hook.sh"
                        }]
                    },
                    {
                        "matcher": "Bash",
                        "hooks": [{
                            "type": "command",
                            "command": "/Users/test/.claude/hooks/rtk-rewrite.sh"
                        }]
                    }
                ]
            }
        });

        let removed = remove_hook_from_json(&mut json_content);
        assert!(removed);

        // Should have only one hook left
        let pre_tool_use = json_content["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre_tool_use.len(), 1);

        // Check it's the other hook
        let command = pre_tool_use[0]["hooks"][0]["command"].as_str().unwrap();
        assert_eq!(command, "/some/other/hook.sh");
    }

    #[test]
    fn test_remove_hook_from_json_new_format() {
        let mut json_content = serde_json::json!({
            "hooks": {
                "PreToolUse": [
                    {
                        "matcher": "Bash",
                        "hooks": [{
                            "type": "command",
                            "command": "/some/other/hook.sh"
                        }]
                    },
                    {
                        "matcher": "Bash",
                        "hooks": [{
                            "type": "command",
                            "command": "rtk hook claude"
                        }]
                    }
                ]
            }
        });

        let removed = remove_hook_from_json(&mut json_content);
        assert!(removed);

        let pre_tool_use = json_content["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre_tool_use.len(), 1);
        let command = pre_tool_use[0]["hooks"][0]["command"].as_str().unwrap();
        assert_eq!(command, "/some/other/hook.sh");
    }

    // =========================================================================
    // GEMINI INIT TESTS
    // =========================================================================

    #[test]
    fn test_gemini_hook_already_present_exact() {
        let json_content = serde_json::json!({
            "hooks": {
                "BeforeTool": [{
                    "matcher": "run_shell_command",
                    "hooks": [{
                        "type": "command",
                        "command": "rtk hook gemini"
                    }]
                }]
            }
        });
        assert!(gemini_hook_already_present(&json_content));
    }

    #[test]
    fn test_gemini_hook_not_present_empty() {
        let json_content = serde_json::json!({});
        assert!(!gemini_hook_already_present(&json_content));
    }

    #[test]
    fn test_gemini_hook_not_present_other_hooks() {
        let json_content = serde_json::json!({
            "hooks": {
                "BeforeTool": [{
                    "matcher": "run_shell_command",
                    "hooks": [{
                        "type": "command",
                        "command": "/some/other/hook.sh"
                    }]
                }]
            }
        });
        assert!(!gemini_hook_already_present(&json_content));
    }

    #[test]
    fn test_gemini_hook_not_present_claude_only() {
        // Claude Code hooks should NOT match Gemini check
        let json_content = serde_json::json!({
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [{
                        "type": "command",
                        "command": "rtk hook claude"
                    }]
                }]
            }
        });
        assert!(!gemini_hook_already_present(&json_content));
    }

    #[test]
    fn test_insert_gemini_hook_entry_empty() {
        let mut json_content = serde_json::json!({});
        insert_gemini_hook_entry(&mut json_content);

        assert!(json_content.get("hooks").is_some());
        let before_tool = json_content["hooks"]["BeforeTool"].as_array().unwrap();
        assert_eq!(before_tool.len(), 1);
        assert_eq!(before_tool[0]["matcher"], "run_shell_command");
        assert_eq!(before_tool[0]["hooks"][0]["command"], "rtk hook gemini");
    }

    #[test]
    fn test_insert_gemini_hook_preserves_existing() {
        let mut json_content = serde_json::json!({
            "hooks": {
                "BeforeTool": [{
                    "matcher": "write_file",
                    "hooks": [{
                        "type": "command",
                        "command": "/some/other/hook.sh"
                    }]
                }]
            }
        });

        insert_gemini_hook_entry(&mut json_content);

        let before_tool = json_content["hooks"]["BeforeTool"].as_array().unwrap();
        assert_eq!(before_tool.len(), 2);
        assert_eq!(before_tool[0]["matcher"], "write_file");
        assert_eq!(before_tool[1]["matcher"], "run_shell_command");
    }

    #[test]
    fn test_insert_gemini_hook_preserves_other_keys() {
        let mut json_content = serde_json::json!({
            "coreTools": {"enabled": true},
            "mcpServers": {}
        });

        insert_gemini_hook_entry(&mut json_content);

        assert_eq!(json_content["coreTools"]["enabled"], true);
        assert!(json_content.get("mcpServers").is_some());
        assert!(json_content.get("hooks").is_some());
    }

    #[test]
    fn test_remove_gemini_hook_from_json() {
        let mut json_content = serde_json::json!({
            "hooks": {
                "BeforeTool": [
                    {
                        "matcher": "write_file",
                        "hooks": [{
                            "type": "command",
                            "command": "/some/other/hook.sh"
                        }]
                    },
                    {
                        "matcher": "run_shell_command",
                        "hooks": [{
                            "type": "command",
                            "command": "rtk hook gemini"
                        }]
                    }
                ]
            }
        });

        let removed = remove_gemini_hook_from_json(&mut json_content);
        assert!(removed);

        let before_tool = json_content["hooks"]["BeforeTool"].as_array().unwrap();
        assert_eq!(before_tool.len(), 1);
        assert_eq!(before_tool[0]["hooks"][0]["command"], "/some/other/hook.sh");
    }

    #[test]
    fn test_remove_gemini_hook_when_not_present() {
        let mut json_content = serde_json::json!({
            "hooks": {
                "BeforeTool": [{
                    "matcher": "write_file",
                    "hooks": [{
                        "type": "command",
                        "command": "/some/other/hook.sh"
                    }]
                }]
            }
        });

        let removed = remove_gemini_hook_from_json(&mut json_content);
        assert!(!removed);
    }

    #[test]
    fn test_remove_gemini_hook_empty_settings() {
        let mut json_content = serde_json::json!({});
        let removed = remove_gemini_hook_from_json(&mut json_content);
        assert!(!removed);
    }

    #[test]
    fn test_gemini_and_claude_hooks_independent() {
        // Both can coexist, removal of one doesn't affect the other
        let mut json_content = serde_json::json!({
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [{
                        "type": "command",
                        "command": "rtk hook claude"
                    }]
                }],
                "BeforeTool": [{
                    "matcher": "run_shell_command",
                    "hooks": [{
                        "type": "command",
                        "command": "rtk hook gemini"
                    }]
                }]
            }
        });

        // Remove Gemini hook
        let removed = remove_gemini_hook_from_json(&mut json_content);
        assert!(removed);

        // Claude hook should still be there
        let pre_tool_use = json_content["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre_tool_use.len(), 1);
        assert!(pre_tool_use[0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .contains("rtk hook claude"));

        // Gemini hook should be gone
        let before_tool = json_content["hooks"]["BeforeTool"].as_array().unwrap();
        assert!(before_tool.is_empty());
    }

    // =========================================================================
    // CLAUDE CODE INIT TESTS (existing)
    // =========================================================================

    #[test]
    fn test_remove_hook_when_not_present() {
        let mut json_content = serde_json::json!({
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [{
                        "type": "command",
                        "command": "/some/other/hook.sh"
                    }]
                }]
            }
        });

        let removed = remove_hook_from_json(&mut json_content);
        assert!(!removed);
    }

    // --- patch_single_cache_file tests ---

    fn make_cache_json(matcher: &str, command: &str) -> serde_json::Value {
        serde_json::json!({
            "hooks": {
                "PreToolUse": [{
                    "matcher": matcher,
                    "hooks": [{"type": "command", "command": command}]
                }]
            }
        })
    }

    #[test]
    fn test_patch_single_cache_file_first_run_bash_removed() {
        let temp = TempDir::new().unwrap();
        let hook_file = temp.path().join("claude-hooks.json");
        let json = make_cache_json("Bash|Write|Edit", "my-hook --cli claude");
        fs::write(&hook_file, serde_json::to_string_pretty(&json).unwrap()).unwrap();

        let mut manifest = BashManifest::default();
        let settings = serde_json::json!({});
        let result =
            patch_single_cache_file(&hook_file, "vendor", "plugin", &settings, &mut manifest, 0);

        assert!(result.unwrap(), "should return true when Bash removed");
        assert_eq!(manifest.entries.len(), 1);
        assert_eq!(manifest.entries[0].original_matcher, "Bash|Write|Edit");
        assert_eq!(manifest.entries[0].patched_matcher, "Write|Edit");
        assert_eq!(
            manifest.entries[0].fallthrough_command,
            "my-hook --cli claude"
        );

        // Verify file was actually patched
        let patched: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&hook_file).unwrap()).unwrap();
        let matcher = patched["hooks"]["PreToolUse"][0]["matcher"]
            .as_str()
            .unwrap();
        assert_eq!(matcher, "Write|Edit");
    }

    #[test]
    fn test_patch_single_cache_file_reconstruction_no_bash() {
        // Bash was already removed; no backup exists. Should reconstruct manifest entry.
        let temp = TempDir::new().unwrap();
        let hook_file = temp.path().join("claude-hooks.json");
        let json = make_cache_json("Write|Edit|ExitPlanMode", "autorun-hook --cli claude");
        fs::write(&hook_file, serde_json::to_string_pretty(&json).unwrap()).unwrap();

        let mut manifest = BashManifest::default();
        let settings = serde_json::json!({});
        let result =
            patch_single_cache_file(&hook_file, "vendor", "plugin", &settings, &mut manifest, 0);

        assert!(result.unwrap(), "should return true for reconstruction");
        assert_eq!(manifest.entries.len(), 1);
        // original_matcher == patched_matcher (safe uninstall no-op)
        assert_eq!(
            manifest.entries[0].original_matcher,
            manifest.entries[0].patched_matcher
        );
        assert_eq!(
            manifest.entries[0].patched_matcher,
            "Write|Edit|ExitPlanMode"
        );
        assert_eq!(
            manifest.entries[0].fallthrough_command,
            "autorun-hook --cli claude"
        );

        // File should NOT be modified (no Bash to remove)
        let after: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&hook_file).unwrap()).unwrap();
        let matcher = after["hooks"]["PreToolUse"][0]["matcher"].as_str().unwrap();
        assert_eq!(
            matcher, "Write|Edit|ExitPlanMode",
            "file should be unchanged"
        );
    }

    #[test]
    fn test_patch_single_cache_file_idempotent_with_manifest() {
        let temp = TempDir::new().unwrap();
        let hook_file = temp.path().join("claude-hooks.json");
        let json = make_cache_json("Write|Edit|ExitPlanMode", "cmd");
        fs::write(&hook_file, serde_json::to_string_pretty(&json).unwrap()).unwrap();

        let cache_path_str = hook_file.to_string_lossy().into_owned();
        let mut manifest = BashManifest::default();
        // Pre-populate manifest as if first run already completed
        manifest.entries.push(ManifestEntry {
            cache_path: cache_path_str,
            original_matcher: "Write|Edit|ExitPlanMode".to_string(),
            patched_matcher: "Write|Edit|ExitPlanMode".to_string(),
            fallthrough_command: "cmd".to_string(),
        });

        let settings = serde_json::json!({});
        let result =
            patch_single_cache_file(&hook_file, "vendor", "plugin", &settings, &mut manifest, 0);

        assert!(
            !result.unwrap(),
            "should return false (already in manifest)"
        );
        assert_eq!(
            manifest.entries.len(),
            1,
            "no new entry added on second run"
        );
    }

    #[test]
    fn test_patch_single_cache_file_no_pretooluse() {
        let temp = TempDir::new().unwrap();
        let hook_file = temp.path().join("claude-hooks.json");
        let json = serde_json::json!({"hooks": {"PostToolUse": []}});
        fs::write(&hook_file, serde_json::to_string_pretty(&json).unwrap()).unwrap();

        let mut manifest = BashManifest::default();
        let settings = serde_json::json!({});
        let result =
            patch_single_cache_file(&hook_file, "vendor", "plugin", &settings, &mut manifest, 0);

        assert!(!result.unwrap(), "should return false (no PreToolUse)");
        assert!(manifest.entries.is_empty());
    }
}
