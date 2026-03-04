# Hook PR Comparison: #156 vs #150 vs #241

## PRs Covered

| PR | Title | Author | Branch |
|----|-------|--------|--------|
| [#156](https://github.com/rtk-ai/rtk/pull/156) | Hook Engine + Chained Command Rewriting (Part 1) | ahundt | `feat/rust-hooks-v2` |
| [#150](https://github.com/rtk-ai/rtk/pull/150) | Native Cross-Platform Hook-Rewrite Command | ayoub-khemissi | `feat/native-hook-rewrite` |
| [#241](https://github.com/rtk-ai/rtk/pull/241) | `rtk rewrite` — Single Source of Truth for LLM Hook Rewrites | FlorianBruniaux | `feat/rtk-rewrite` |

## PR Series: #156 Unlocks One Follow-On

| PR | Title | Depends on | Status |
|----|-------|------------|--------|
| **#156** | Hook Engine + Chained Command Rewriting (Part 1) | standalone | open, awaiting re-review |
| **#157** | Data Safety Rules (separate direction, excluded from this analysis) | #156 | — |
| **#158** | Gemini CLI Hook Support (Part 3) | #156 | open, needs rebase |

---

## Root Cause: Why Plugin Cache Patching Matters

### This bug affects master today

RTK on `master` is already correctly installed: `settings.json` has an RTK entry, the binary
is invoked, and it produces valid `updatedInput` JSON. Yet `git status` is not rewritten to
`rtk git status` in live Claude Code sessions. The hook appears to work but silently does
nothing.

**Why**: Claude Code has a bug where, when two hooks from **different sources** both match
`Bash` in `PreToolUse`, Claude Code receives two responses in parallel and **silently drops
`updatedInput` from both** — even when one response contains a valid rewrite and both responses
allow the command. The two sources here are:

1. **`settings.json`** (user config) — RTK's hook entry
2. **Plugin cache** (`~/.claude/plugins/cache/autorun/ar/0.9.0/hooks/claude-hooks.json`) —
   the `autorun` plugin, which also matches `Bash` to enforce safety rules

Claude Code's documented "Restrictive Wins" rule covers only `permissionDecision` merge (deny
beats allow). What happens to `updatedInput` when two hooks both allow but one has a rewrite
is **undocumented and broken in practice**: the rewrite is dropped.

**Concrete evidence from investigation**:

RTK produces correct JSON (`/tmp/rtk_stdout.txt`):
```json
{
  "hookSpecificOutput": {
    "hookEventName": "PreToolUse",
    "permissionDecision": "allow",
    "updatedInput": { "command": "rtk git status" }
  }
}
```

RTK IS invoked (`/tmp/rtk-hook-trace.log`):
```
13:38:28 rtk-hook-invoked which=/Users/athundt/.cargo/bin/rtk
```

But `git status` runs unmodified. The rewrite is produced and discarded.

This means **every RTK user who also has the `autorun` plugin installed gets 0% token savings
from the hook**, silently, with no error or warning. The same applies to any other Claude Code
plugin that registers a `Bash` PreToolUse matcher alongside RTK.

### Why the obvious workarounds don't work

**`sequential: true` in `settings.json`**: Claude Code strips unknown JSON fields when it
rewrites `settings.json` (e.g. whenever the `/model` command runs). Confirmed in practice — the
field disappears after the next settings rewrite. Non-durable.

**Ordering the hooks differently**: The two hook sources (`settings.json` entries vs plugin
cache entries) fire in parallel regardless of their order within a single source. There is no
supported way to order across sources.

### The fix: make RTK the sole Bash hook responder

1. Remove `Bash` from the PreToolUse matcher in each plugin's cache file
   (`~/.claude/plugins/cache/*/hooks/*.json`)
2. RTK binary reads a manifest of displaced handlers and spawns them itself when it has no
   rewrite opinion — so autorun's safety rules (`rm -rf` blocking, tool suggestions, etc.) are
   still enforced via fallthrough

**Only #156 implements this.** `rtk init` patches all plugin caches, writes
`~/.claude/hooks/rtk-bash-manifest.json`, and `rtk hook claude` spawns fallthrough handlers
when it has no rewrite opinion. Neither #150 nor #241 address this — both assume RTK is already
the sole Bash hook, which is only true if the user has no other plugins with Bash matchers.
Any user with `autorun` (or any future plugin with a Bash matcher) gets silent 0% savings.

---

## Capability Comparison

| Capability | #156 | #150 | #241 | Why It Matters |
|------------|:---:|:---:|:---:|----|
| **Hook protocol** | | | | |
| Reads PreToolUse JSON from stdin | ✅ | ✅ | ❌ bash owns this | Pure Rust avoids bash/jq dependency; works natively on Windows |
| Writes `updatedInput` JSON to stdout | ✅ | ✅ | ❌ bash owns this | Required for Claude Code to apply the rewrite |
| Fail-open on malformed input (exit 0) | ✅ | ✅ | ✅ | Bad JSON from Claude Code must never block the user |
| **Command rewriting** | | | | |
| Quote-aware lexer for chain splitting | ✅ `lexer.rs` | ✅ `hook/mod.rs` | ✅ `rewrite_compound` | `git commit -m "Fix && Bug"` must NOT split on `&&` inside quotes |
| `&&` / `\|\|` / `;` chain rewriting | ✅ | ✅ | ✅ | Closes [#112](https://github.com/rtk-ai/rtk/issues/112): `cd /tmp && git status` previously only rewrote `cd` |
| Pipe rewriting (`git log \| grep`) | ✅ suffix-aware | ✅ | ❌ first segment only | `cargo test \| grep FAILED` → `rtk cargo test \| rtk grep FAILED`; second segment also filtered |
| Env-prefix preservation (`TEST=1 git`) | ✅ | ✅ | ✅ | Common CI pattern; env vars must not be stripped or lost |
| Routes to specialized RTK filters | ✅ `registry.rs` | ✅ `src/hook/*.rs` | ✅ `registry.rs` | Core RTK value: `cargo test` → 90% token reduction; bypassing filters → 0% (measured in [#156 review](https://github.com/rtk-ai/rtk/pull/156#issuecomment-3923337082)) |
| **Execution** | | | | |
| Streaming output (line-by-line) | ✅ `exec.rs` | ❌ pure rewriter | ❌ bash runs cmd | `cargo build --release` takes 2+ min; buffering until completion shows no output — flagged by pszymkowiak in [#156 review](https://github.com/rtk-ai/rtk/pull/156#issuecomment-3923337082) |
| Redirect support (`>`, `>>`) | ✅ | ❌ | ❌ | Must not silently break or misdirect output |
| Builtin handling (`cd`, `export`) | ✅ `builtins.rs` | ❌ | ❌ | `cd` changes working dir for subsequent chained commands; cannot be exec'd as a child process |
| **Init / install** | | | | |
| Plugin cache patching (sole Bash hook) | ✅ | ❌ | ❌ | **See root cause above** — without this, `updatedInput` is silently dropped when any other plugin also handles Bash |
| Manifest fallthrough (preserves other plugin rules) | ✅ | ❌ | ❌ | Without fallthrough, patching autorun's cache disables its safety rules (`rm -rf` blocking, tool suggestions, etc.) |
| Backup registry (`rtk-backups.json`) | ✅ | ❌ | ❌ | Idempotent `rtk init` re-runs; user can find original files without manual filesystem search |
| **Multi-LLM** | | | | |
| Gemini CLI hook (`rtk hook gemini`) | ✅ via [#158](https://github.com/rtk-ai/rtk/pull/158) | ❌ | ❌ | Different wire format (`BeforeTool`/`decision` vs `PreToolUse`/`permissionDecision`); 65% shared infra claimed in PR description |
| Any hook can call `rtk rewrite` as oracle | ❌ | ❌ | ✅ | Non-Rust hooks (bash, Python) can call `rtk rewrite "cmd"` and get back the rewritten string — e.g. Gemini hook in ~15 lines of bash |
| **Platform** | | | | |
| Windows (no bash required for hook) | ✅ | ✅ | ❌ hook is bash | Windows users need Git Bash or WSL to run a bash hook |
| **Testing** | | | | |
| Unit tests passing | 814 | 524 | 455 | |
| Real execution test (real toolchains) | ✅ E2E tests; 98/98 by pszymkowiak | ✅ 98/98 by pszymkowiak | ❌ not done | Routing regression (0% vs 90% savings) was only caught by real execution testing, not unit tests |
| **Review state** | | | | |
| Reviewer positive on architecture | ✅ see quotes below | ✅ see quotes below | ✅ see quotes below | |
| Bugs from initial review fixed | ✅ routing fix Feb 22; streaming, pipe routing added | ✅ `git -C` and `grep -r` fixed Mar 2 | ❌ 3 bugs open since Mar 2 | |
| Reviewer has seen post-fix version | ❌ pszymkowiak only asked for rebase Mar 2, no re-review | ❌ same — fix landed Mar 2, no re-review | ❌ bugs still open | All three PRs are effectively waiting |

---

## Size

| PR | Lines added vs master | Core hook code |
|----|----------------------|----------------|
| **#156** | +9,441 | `lexer.rs` + `analysis.rs` + `exec.rs` + `claude_hook.rs` + `builtins.rs` = 2,695 lines |
| **#150** | +2,864 | `src/hook/` (9 files) = 1,528 lines |
| **#241** | +1,733 | `rewrite_cmd.rs` = 47 lines (delegates to existing `registry.rs`) |

---

## Reviewer Quotes (verbatim)

**pszymkowiak on [#156](https://github.com/rtk-ai/rtk/pull/156#issuecomment-3923337082)** (Feb 18 — before routing fix):
> "the architecture is genuinely well thought out — the lexer, fail-open design, RAII guard,
> and deny(clippy::print_stdout) are all excellent."
> "The foundation here is solid. The lexer, chain parsing, and fail-open protocol handling are
> exactly what we need."

The initial routing regression (commands routed through `rtk run -c` instead of specialized
filters → 0% savings) was fixed by Feb 22. Since then: registry-based routing, streaming
execution, pipe routing with suffix-aware rewriting, and E2E tests were added. pszymkowiak's
Mar 2 comment was only a rebase request — he has not reviewed any of the post-fix improvements.

**pszymkowiak on [#150](https://github.com/rtk-ai/rtk/pull/150#issuecomment-3930218186)** (Feb 20 — after split into 9 modules):
> Ran 98/98 real execution tests across all ecosystems (Rust, Node.js, Python, Go, Git, GitHub
> CLI, Docker/K8s, Files, Network, Meta). "All RTK filters work perfectly with real command
> output. Zero regressions vs master."

aeppling (Feb 19): "Approved. Code is correct for me." (Before module split.)
Two bugs found Mar 2 (`git -C` silent wrong directory; `grep -r` clap arg ordering), both
fixed same day. pszymkowiak has not reviewed the fixes.

**pszymkowiak on [#241](https://github.com/rtk-ai/rtk/pull/241#issuecomment-2692148567)** (Mar 2):
> "rtk rewrite as a single source of truth is exactly the right direction, and the Rust
> implementation of rewrite_command() is solid."

Three bugs identified blocking merge (see below). FlorianBruniaux has not fixed them as of
this writing.

---

## Bugs Blocking #241 (identified by pszymkowiak, Mar 2 — unfixed)

| Bug | Symptom | Fix needed |
|-----|---------|------------|
| `rtk init` installs old 218-line hook | Users who run `rtk init` after merge never get the new thin delegating hook | Update embedded `hooks/rtk-rewrite.sh` to the thin version |
| `head -20 file` crashes | `head -20 src/main.rs` → rewritten to `rtk read -20 src/main.rs` → clap parse error at runtime | Skip rewriting `head` when numeric flags present, or translate `-N` to `--max-lines N` |
| Silent failure with older RTK binary | If installed RTK predates 0.23.0, `rtk rewrite` doesn't exist → hook does `\|\| exit 0` → zero rewrites, no error, no warning | Add version guard or explicit fallback message |

Also noted as missing from registry: `gh release`, `kubectl describe`/`apply`, `docker run`/`exec`/`build`, `cargo install`, `tree`, `diff`.

---

## Architectural Question

**Does RTK own the hook protocol, or is RTK a rewrite oracle?**

- **Protocol-owner** (#156, #150): RTK reads JSON stdin, writes JSON stdout directly.
  Fail-open, streaming (in #156), Windows-native. Each new LLM needs a new protocol handler
  in Rust (~35% new code per LLM using shared infra, per #156 PR description). Enables
  plugin cache patching and manifest fallthrough to fix the parallel-hook bug.
- **Oracle** (#241): RTK prints the rewritten command string; each LLM's hook owns its own
  JSON protocol. Any LLM gets integration in ~15 lines of bash. Windows still requires bash.
  Plugin cache patching not addressed.

These are different integration styles, not necessarily mutually exclusive: `rtk hook claude`
(protocol-owner for Claude) and `rtk rewrite` (oracle for other tools) could coexist. The
practical question is which becomes the primary mechanism for Claude Code, since only the
protocol-owner style can fix the parallel-hook `updatedInput` drop bug.

### What #156 has that neither #150 nor #241 have
- Streaming execution (no buffering — important for long-running commands)
- Redirect support (`>`, `>>`)
- Builtin handling (`cd` in chains)
- Plugin cache patching + manifest fallthrough (the only PR that fixes the parallel-hook bug)
- Unblocks Gemini multi-LLM support via [#158](https://github.com/rtk-ai/rtk/pull/158)

### Worktree locations (for local comparison)

| PR | Worktree |
|----|----------|
| #156 | `.worktrees/pr-rust-hooks-v2` |
| #150 | `.worktrees/pr-native-hook-rewrite` |
| #241 | `.worktrees/pr-rtk-rewrite` |
