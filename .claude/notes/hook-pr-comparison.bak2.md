# Hook PR Comparison: #156 vs #150 vs #241

## Summary

| PR | Author | Approach | Key Strength | Key Gap | Review Status |
|----|--------|----------|--------------|---------|---------------|
| [#156](https://github.com/rtk-ai/rtk/pull/156) | ahundt | RTK owns hook protocol; full execution pipeline | Streaming, chains, plugin cache patching, multi-LLM foundation | Largest; pszymkowiak hasn't reviewed post-routing-fix version | Awaiting re-review |
| [#150](https://github.com/rtk-ai/rtk/pull/150) | ayoub-khemissi | RTK owns hook protocol; pure rewriter | 4 review cycles, 98/98 real-execution tested, modular | No streaming, no plugin cache patching | Awaiting re-review after Mar 2 bug fixes |
| [#241](https://github.com/rtk-ai/rtk/pull/241) | FlorianBruniaux | RTK is a rewrite oracle; bash owns protocol | Smallest; any LLM integrates with ~15 lines of bash | Hook stays bash (no Windows native); plugin cache patching not addressed; 3 review-blocking bugs open | Blocked on unfixed bugs |

---

## Background: What These PRs Are Trying to Fix

RTK's value is routing shell commands through specialized filters:
`cargo test` → `rtk cargo test` → shows failures only → **90% token reduction**
`git log -10` → `rtk git log -10` → compressed output → **85% token reduction**

The hook is what makes this transparent — Claude Code runs `git status` and the hook rewrites
it to `rtk git status` before execution. Without the hook working, the user must type `rtk`
manually every time.

---

## The Parallel-Hook Bug: Why Hook Rewrites Silently Fail on Master Today

### What users experience

RTK is correctly installed on `master`. `settings.json` has the RTK hook entry. The binary is
invoked. It produces valid output. Yet `git status` runs as-is — the rewrite to `rtk git
status` never happens. No error, no warning.

### Why it happens

Two independent sources both register a `PreToolUse` hook matching `Bash`:

| Source | Entry | Purpose |
|--------|-------|---------|
| `~/.claude/settings.json` | RTK hook | Rewrites commands, emits `updatedInput` JSON |
| `~/.claude/plugins/cache/autorun/ar/0.9.0/hooks/claude-hooks.json` | autorun plugin | Enforces safety rules (`rm -rf` blocking, tool suggestions) |

Claude Code fires both in parallel and receives two responses. Its documented "Restrictive
Wins" rule covers only `permissionDecision` merging (deny beats allow). What happens to
`updatedInput` when both hooks allow but only one has a rewrite is **undocumented** — in
practice, Claude Code drops the rewrite.

This affects **every RTK user who has the `autorun` plugin installed**, silently.
The same failure occurs with any other Claude Code plugin that registers a `Bash` matcher.

### Evidence

RTK produces correct output (from `/tmp/rtk_stdout.txt`):
```json
{
  "hookSpecificOutput": {
    "hookEventName": "PreToolUse",
    "permissionDecision": "allow",
    "updatedInput": { "command": "rtk git status" }
  }
}
```

RTK is invoked (from `/tmp/rtk-hook-trace.log`):
```
13:38:28 rtk-hook-invoked which=/Users/athundt/.cargo/bin/rtk
```

The output is correct. The binary runs. The rewrite disappears anyway.

### Why the obvious workarounds fail

**`sequential: true` in `settings.json`**: Claude Code strips unrecognized JSON fields when it
rewrites `settings.json` (confirmed: field disappears after the `/model` command runs).
Non-durable.

**Reordering hooks within `settings.json`**: Plugin cache hooks fire in parallel with
`settings.json` hooks regardless of ordering within either source. There is no supported way
to sequence hooks across sources.

### The fix

Make RTK the sole `Bash` PreToolUse responder:
1. Remove `Bash` from the matcher in each plugin's cache file
   (`~/.claude/plugins/cache/*/hooks/*.json`)
2. When RTK has no rewrite opinion, it reads a manifest of displaced plugin handlers and
   spawns them as subprocesses — so autorun's safety rules remain enforced

**Only [#156](https://github.com/rtk-ai/rtk/pull/156) implements the parallel-hook fix.**
`rtk init` patches all plugin caches, writes `~/.claude/hooks/rtk-bash-manifest.json`, and
`rtk hook claude` spawns fallthrough handlers when it has no rewrite opinion.

[#150](https://github.com/rtk-ai/rtk/pull/150) and [#241](https://github.com/rtk-ai/rtk/pull/241)
do not address the parallel-hook bug. Both assume RTK is already the sole `Bash` hook, which
is only true if the user has no other plugins with `Bash` matchers. For any user with
`autorun` or a similar plugin, hook rewrites silently produce 0% savings after merging either
of those PRs.

---

## Capability Comparison

| Capability | [#156](https://github.com/rtk-ai/rtk/pull/156) | [#150](https://github.com/rtk-ai/rtk/pull/150) | [#241](https://github.com/rtk-ai/rtk/pull/241) | Why It Matters |
|------------|:---:|:---:|:---:|----------------|
| **Hook protocol** | | | | |
| RTK reads PreToolUse JSON, writes `updatedInput` | ✅ | ✅ | ❌ bash does this | Pure Rust: no bash/jq deps; works natively on Windows |
| Fail-open on malformed input (exit 0) | ✅ | ✅ | ✅ | Bad input from Claude Code must never block the user |
| **Command rewriting** | | | | |
| Quote-aware chain splitting | ✅ `lexer.rs` | ✅ `hook/mod.rs` | ✅ `rewrite_compound` | `git commit -m "Fix && Bug"` must not split on `&&` inside quotes |
| `&&` / `\|\|` / `;` chain rewriting | ✅ | ✅ | ✅ | Closes [#112](https://github.com/rtk-ai/rtk/issues/112): `cd /tmp && git status` previously only rewrote `cd` |
| Pipe rewriting (`git log \| grep`) | ✅ suffix-aware | ✅ | ❌ first segment only | `cargo test \| grep FAILED` → `rtk cargo test \| rtk grep FAILED`; both segments filtered |
| Env-prefix preservation (`TEST=1 git`) | ✅ | ✅ | ✅ | Common CI pattern; env vars must survive the rewrite |
| Routes to specialized RTK filters | ✅ `registry.rs` | ✅ `src/hook/*.rs` | ✅ `registry.rs` | The routing regression (0% savings) was the first critical bug on both #156 and #150; now fixed in both |
| **Execution** | | | | |
| Streaming output (line-by-line) | ✅ `exec.rs` | ❌ pure rewriter | ❌ bash runs cmd | `cargo build --release` takes 2+ min; buffering to completion was a UX regression flagged in [#156 review](https://github.com/rtk-ai/rtk/pull/156#issuecomment-3923337082) |
| Redirect support (`>`, `>>`) | ✅ | ❌ | ❌ | Must not silently break or misdirect output |
| Builtin handling (`cd`, `export`) | ✅ `builtins.rs` | ❌ | ❌ | `cd` changes the working directory for subsequent chained commands and cannot be exec'd as a child process |
| **Parallel-hook bug fix** | | | | |
| Patches plugin caches to make RTK the sole `Bash` hook | ✅ | ❌ | ❌ | Without this, `updatedInput` is silently dropped when any other plugin also handles `Bash` — see root cause section above |
| Manifest fallthrough (other plugin rules still enforced) | ✅ | ❌ | ❌ | Without fallthrough, patching autorun's cache disables its `rm -rf` blocking and tool suggestions |
| Backup registry (`rtk-backups.json`) | ✅ | ❌ | ❌ | Idempotent `rtk init` re-runs; user can locate original files without filesystem searching |
| **Multi-LLM** | | | | |
| Gemini CLI hook (`rtk hook gemini`) | ✅ via [#158](https://github.com/rtk-ai/rtk/pull/158) | ❌ | ❌ | Gemini uses different wire format (`BeforeTool`/`decision` vs `PreToolUse`/`permissionDecision`); #158 shares 65% of #156's infrastructure (per PR description) |
| Any LLM hook calls `rtk rewrite` as oracle | ❌ | ❌ | ✅ | Non-Rust hooks (bash, Python) call `rtk rewrite "cmd"` → get rewritten string back; Gemini hook in ~15 lines of bash |
| **Platform** | | | | |
| Windows without bash | ✅ | ✅ | ❌ hook is bash | Windows users need Git Bash or WSL to run a bash hook |
| **Testing** | | | | |
| Unit tests passing | 814 | 524 | 455 | |
| Real execution tested against actual toolchains | ✅ E2E tests added | ✅ 98/98 by pszymkowiak | ❌ | The routing regression (0% savings instead of 90%) was only caught by real execution testing |
| **Review state** | | | | |
| All reviewer-identified bugs fixed | ✅ fixed by Feb 22 | ✅ fixed by Mar 2 | ❌ 3 open | |
| Reviewer has seen the fixed version | ❌ pszymkowiak has not re-reviewed | ❌ pszymkowiak has not re-reviewed | ❌ bugs still open | All three PRs are effectively stalled waiting for pszymkowiak |

---

## Size

| PR | Lines added vs master | Core hook files |
|----|----------------------|-----------------|
| **#156** | +9,441 | `lexer.rs` + `analysis.rs` + `exec.rs` + `claude_hook.rs` + `builtins.rs` = 2,695 lines |
| **#150** | +2,864 | `src/hook/` (9 files) = 1,528 lines |
| **#241** | +1,733 | `rewrite_cmd.rs` = 47 lines (delegates to existing `registry.rs`) |

---

## Open Review-Blocking Bugs in #241

Identified by pszymkowiak [Mar 2](https://github.com/rtk-ai/rtk/pull/241#issuecomment-2692148567).
FlorianBruniaux has not fixed them as of this writing.

| Bug | Symptom | Fix needed |
|-----|---------|------------|
| `rtk init` installs old 218-line bash hook | Users who run `rtk init` after merge never get the new thin delegating hook — the whole point of the PR | Update embedded `hooks/rtk-rewrite.sh` to the thin delegating version |
| `head -20 file` crashes at runtime | `head -20 src/main.rs` → rewritten to `rtk read -20 src/main.rs` → clap parse error | Skip rewriting `head` when a numeric flag is present, or translate `-N` to `--max-lines N` |
| Silent failure with RTK < 0.23.0 | If installed RTK predates 0.23.0, `rtk rewrite` doesn't exist → hook does `\|\| exit 0` → zero rewrites, no error, no warning to the user | Add a version guard or an explicit fallback message |

Also noted as missing from the registry (present in the old bash hook but absent from
`registry.rs`): `gh release`, `kubectl describe`/`apply`, `docker run`/`exec`/`build`,
`cargo install`, `tree`, `diff`.

---

## Reviewer Quotes (verbatim)

**pszymkowiak on [#156](https://github.com/rtk-ai/rtk/pull/156#issuecomment-3923337082)** (Feb 18 — before routing fix):
> "the architecture is genuinely well thought out — the lexer, fail-open design, RAII guard,
> and deny(clippy::print_stdout) are all excellent."
> "The foundation here is solid. The lexer, chain parsing, and fail-open protocol handling are
> exactly what we need."

The original criticism was a routing regression where all commands went through `rtk run -c`
instead of specialized filters, yielding 0% savings (measured). Fixed by Feb 22. Since then:
registry-based routing, streaming execution, suffix-aware pipe routing, and E2E tests were
added. pszymkowiak's only Mar 2 comment was a rebase request — he has not reviewed any of
the post-fix work.

**pszymkowiak on [#150](https://github.com/rtk-ai/rtk/pull/150#issuecomment-3930218186)** (Feb 20):
> Ran 98/98 real-execution tests across all ecosystems. "All RTK filters work perfectly with
> real command output. Zero regressions vs master."

aeppling (Feb 19): "Approved. Code is correct for me." Two bugs found Mar 2 (`git -C` silent
wrong-directory rewrite; `grep -r` clap argument ordering crash), both fixed same day.
pszymkowiak has not reviewed the fixes.

**pszymkowiak on [#241](https://github.com/rtk-ai/rtk/pull/241#issuecomment-2692148567)** (Mar 2):
> "rtk rewrite as a single source of truth is exactly the right direction, and the Rust
> implementation of rewrite_command() is solid."

Followed by three review-blocking bugs (listed above). FlorianBruniaux has not responded.

---

## Architectural Question

**Does RTK own the hook protocol, or does RTK act as a rewrite oracle?**

**Protocol-owner** (#156, #150): RTK reads the LLM's JSON from stdin and writes `updatedInput`
JSON to stdout. The hook is just `"command": "rtk hook claude"` — no bash logic.
- Works natively on Windows (no bash required)
- Enables the plugin cache patching that fixes the parallel-hook bug
- Each new LLM needs a new protocol handler; #156 claims 65% shared infrastructure across
  Claude and Gemini (per PR description, not independently verified)

**Oracle** (#241): RTK takes a command string and returns the rewritten string.
The hook is ~18 lines of bash that owns the JSON protocol itself.
- Any LLM tool integrates in ~15 lines of bash using `rtk rewrite "cmd"`
- Still requires bash on Windows
- Does not fix the parallel-hook bug

These are different integration styles and could coexist (`rtk hook claude` for Claude,
`rtk rewrite` for bash-based hooks). The practical issue is that only the protocol-owner
style enables the plugin cache patching needed to fix the parallel-hook `updatedInput` drop
bug on master.

---

## Worktree Locations

| PR | Worktree |
|----|----------|
| #156 | `.worktrees/pr-rust-hooks-v2` |
| #150 | `.worktrees/pr-native-hook-rewrite` |
| #241 | `.worktrees/pr-rtk-rewrite` |
