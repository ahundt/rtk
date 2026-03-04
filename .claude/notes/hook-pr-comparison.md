# Hook PR Comparison: #156 vs #150 vs #241

## Summary

| PR | Author | Approach | Key Strength | Key Gap | Review Status |
|----|--------|----------|--------------|---------|---------------|
| [#156](https://github.com/rtk-ai/rtk/pull/156) | ahundt | RTK owns JSON protocol + full execution pipeline | Streaming, plugin cache patching, multi-LLM foundation via #158 | Largest (+9,441 lines); pszymkowiak hasn't reviewed post-routing-fix version | Awaiting re-review |
| [#150](https://github.com/rtk-ai/rtk/pull/150) | ayoub-khemissi | RTK owns JSON protocol; pure string rewriter only | Modular 9-file design; 4 review cycles; 98/98 real-execution tested | No streaming; no plugin cache patching | Awaiting re-review after Mar 2 bug fixes |
| [#241](https://github.com/rtk-ai/rtk/pull/241) | FlorianBruniaux | RTK is a rewrite oracle; bash owns JSON protocol | Smallest (+1,733 lines); any LLM integrates with ~15 lines of bash; audit log | Requires jq+bash (no Windows native); `&`-in-`2>&1` bug (malformed redirect rewrite); review-blocking bugs now fixed | Awaiting re-review |

---

## Assessment: Which PR Is Best

**#156 is the correct choice in both scenarios.** The reasoning changes between scenarios but the conclusion does not.

### Scenario A: Parallel-hook bug present (no independent fix)

Token savings only happen when a rewrite fires. All three PRs route to the same filter registry, so per-command savings are identical once a rewrite succeeds. The difference is whether rewrites succeed at all.

For any user with `autorun` installed: #150 and #241 silently produce 0% savings — the parallel-hook `updatedInput` drop means RTK's rewrite is never applied. #156 is the only PR that fixes this by patching plugin caches and handling fallthrough in Rust.

| PR | Rewrites fire with autorun? | Effective token savings |
|----|---------------------------|------------------------|
| #156 | ✅ plugin cache patched | 60–90% per command |
| #150 | ❌ silently dropped | 0% |
| #241 | ❌ silently dropped | 0% |

### Scenario B: Parallel-hook bug fixed independently

Even with the parallel-hook bug fixed elsewhere, #156 is still the better choice on three grounds that #150 and #241 both fail:

**1. Pipe scripts break with #150 and #241.**

`cargo test | grep FAILED` is not just a token savings question — it is a data correctness question. When #150 or #241 rewrite this to `rtk cargo test | grep FAILED`:
- `rtk cargo test` buffers all output, applies its filter (reformats failures, changes text structure), then emits its summary
- `grep FAILED` receives RTK's summary, not raw cargo output — the literal string "FAILED" may not appear in RTK's output at all
- The grep returns empty or wrong results; the script breaks silently

#156 detects the pipe, recognizes `grep` as a format-sensitive consumer, and routes the whole command through the shell unchanged. The user gets correct grep results at the cost of 0% savings on that specific invocation — which is the right tradeoff. Corrupted grep output that silently returns wrong results is worse than unfiltered output.

For `cargo build --release | tee build.log` (a 5-minute build):
- #150/#241: RTK buffers the entire build internally, dumps everything to tee after 5 minutes — tee is connected but receives nothing until the build finishes
- #156: `| tee` is recognized as a format-agnostic safe suffix; the core command streams live to tee as lines are produced

**2. #150 and #241 cannot actually block commands (Claude Code Bug #4669).**

Claude Code silently ignores a `"deny"` response when the hook exits 0. Both #150 and #241 use exit 0 for all responses including denials. This means:
- If RTK ever needs to block a command, neither #150 nor #241 can enforce it — the block is silently dropped
- Fallthrough handlers (e.g. autorun blocking `rm -rf`) that exit 2 are not propagated — autorun's safety rules do not actually work through #150 or #241 even if the parallel-hook bug is fixed externally

#156 uses exit 2 + stderr for denials, which Claude Code does honor. Manifest fallthrough handlers that exit 2 have their exit code propagated (guarded by the `write_ok` check to avoid false denials on broken pipes).

**3. The implementation complexity gap is smaller than it appears.**

The "3× less code" argument for #150 is misleading:

| | #150 hook files | #156 core hook files |
|--|----------------|---------------------|
| Total lines added | 1,537 | 3,693 |
| Test lines | 757 (49%) | 2,662 (72%) |
| Implementation lines | ~780 | ~1,031 |

The implementation gap is **1.3×**, not 3×. The extra lines in #156 are 3.5× more test coverage. #156 also fixed real bugs during development that #150 did not encounter: a streaming I/O spin on broken pipes (`filter_map` → `map_while`), dead test assertions that were no-ops, and missing dispatch table entries for npm/pnpm/go.

### What #150 does better than #156

#150 is not without merit. In a world where the above three issues were fixed in #150:
- The modular 9-file design is cleaner to audit than #156's execution pipeline
- 4 review cycles with pszymkowiak's 98/98 real-execution validation is strong evidence of correctness for the commands it covers
- No execution engine means a smaller surface for bugs that affect running processes

These are real advantages, but they do not outweigh pipe correctness, deny enforcement, and streaming — all of which affect users on every session.

### What #241 brings that neither #150 nor #156 has

- Oracle model: non-Rust hook integrations need only ~15 lines of bash calling `rtk rewrite`
- `RTK_HOOK_AUDIT=1` audit log built into the hook
- `&` handling with savings for simple trailing `&` (e.g., `cargo build &` → `rtk cargo build &`). **But**: the byte-by-byte parser has a bug where `&` in `2>&1` is misidentified as a background operator → `cargo build 2>&1` → `"rtk cargo build 2> & 1"` (broken redirect). #156 handles `2>&1` correctly via Shellism → shell passthrough. #156 misses savings on simple `cargo build &` but never produces malformed commands.

These are useful capabilities. The oracle model in particular could coexist with #156 as a secondary interface for non-Claude-Code integrations, since `rtk rewrite` and `rtk hook claude` serve different callers.

### Summary

| | Scenario A (autorun installed, no independent fix) | Scenario B (parallel-hook bug fixed separately) |
|--|--------------------------------------------------|--------------------------------------------------|
| **Best** | **#156** — only one where rewrites fire | **#156** — pipe correctness, streaming, deny enforcement |
| **Runner-up** | nothing else works | #150 — cleaner design, same savings on simple commands |
| **Weakest** | #150, #241 — 0% savings silently | #241 — bash+jq required, backtick gap, no deny |
| **Complementary** | — | #241's `rtk rewrite` oracle for non-Claude-Code hooks |

The parallel-hook bug can be fixed independently without merging any of these PRs. Once it is, #150's simpler architecture becomes the strongest choice if streaming execution and multi-LLM support (via #158) are not priorities.

---

## Background: What These PRs Are Trying to Fix

RTK's value is routing shell commands through specialized filters before execution:
```
cargo test  →  rtk cargo test  →  shows failures only  →  90% token reduction
git log -10 →  rtk git log -10 →  compressed output    →  85% token reduction
```

The hook makes this transparent — Claude Code runs `git status`, the hook rewrites it to
`rtk git status` before execution. Without the hook working correctly, the user must type
`rtk` manually every time, which defeats the purpose.

---

## The Parallel-Hook Bug: Why Rewrites Silently Fail on Master Today

### What users experience

RTK is correctly installed on `master`. `settings.json` has the RTK hook entry. The binary is
invoked. It produces valid `updatedInput` JSON. Yet `git status` runs as-is — the rewrite to
`rtk git status` is silently dropped. No error, no warning.

### Why it happens

Two independent sources both register a `PreToolUse` hook matching `Bash`:

| Source | File | Purpose |
|--------|------|---------|
| `~/.claude/settings.json` | User config | RTK hook: rewrites commands, emits `updatedInput` |
| `~/.claude/plugins/cache/autorun/ar/0.9.0/hooks/claude-hooks.json` | autorun plugin cache | Enforces safety rules (`rm -rf` blocking, tool suggestions, etc.) |

Claude Code fires both in parallel and receives two responses. Its documented "Restrictive
Wins" rule covers only `permissionDecision` merging (deny beats allow). What happens to
`updatedInput` when both hooks allow but only one has a rewrite is **undocumented** — in
practice, Claude Code drops the rewrite from both responses.

This affects every RTK user who has the `autorun` plugin installed, silently. The same
failure occurs with any other Claude Code plugin that registers a `Bash` matcher.

### Evidence

RTK produces correct `updatedInput` JSON (verified from `/tmp/rtk_stdout.txt`):
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

The output is correct. The binary runs. The rewrite is dropped by Claude Code anyway.

### Why obvious workarounds fail

**`sequential: true` in `settings.json`**: Claude Code strips unrecognized JSON fields when it
rewrites `settings.json` (confirmed: field disappears after the `/model` command runs).
Non-durable.

**Reordering hooks within `settings.json`**: Plugin cache hooks fire in parallel with
`settings.json` hooks regardless of ordering within either source. There is no supported way
to sequence hooks across sources.

### The fix: make RTK the sole `Bash` PreToolUse responder

1. Remove `Bash` from the matcher in each plugin's cache file
   (`~/.claude/plugins/cache/*/hooks/*.json`)
2. When RTK has no rewrite opinion, it reads a manifest of displaced plugin handlers and
   spawns them as subprocesses with the original payload — so autorun's safety rules remain
   enforced

**Only [#156](https://github.com/rtk-ai/rtk/pull/156) implements the parallel-hook fix.**
`rtk init` patches all plugin caches, writes `~/.claude/hooks/rtk-bash-manifest.json`, and
`rtk hook claude` (`src/cmd/claude_hook.rs`) spawns fallthrough handlers when it has no
rewrite opinion. [#150](https://github.com/rtk-ai/rtk/pull/150) and
[#241](https://github.com/rtk-ai/rtk/pull/241) do not address the parallel-hook bug. Both
assume RTK is already the sole `Bash` hook, which is only true if the user has no other
plugins with `Bash` matchers. For any user with `autorun` or a similar plugin, hook rewrites
silently produce 0% savings after merging either of those PRs.

---

## Design Philosophy

### #156: RTK as a full hook engine

RTK owns the complete hook lifecycle. `src/cmd/claude_hook.rs` reads JSON from stdin, decides
whether to rewrite, and writes `updatedInput` JSON to stdout — no bash involved. The hook
entry in `settings.json` is just `"command": "rtk hook claude"`.

The architecture enforces I/O discipline at compile time: `#![deny(clippy::print_stdout,
clippy::print_stderr)]` in `claude_hook.rs` means stray `println!` calls are **compile
errors**, not runtime bugs. The only I/O path is an explicit `write!` call at the end of
`run()`. This prevents the class of bug where a debug print corrupts the JSON protocol.

Execution is handled inside RTK itself via `src/cmd/exec.rs` (for `rtk run -c` chains).
Decisions live in `src/cmd/hook.rs` (shared between Claude and Gemini). The lexer and
analysis modules are independent of any LLM protocol.

### #150: RTK as a lightweight JSON rewriter

RTK owns the JSON protocol but does no execution — it rewrites the command string and exits.
Claude Code then runs the rewritten command normally. The hook entry is
`"command": "rtk hook-rewrite"`, a hidden subcommand.

The 9-module structure (`src/hook/{git,cargo,files,js_ts,containers,python,go,helpers}.rs`)
keeps each tool's rewriting logic isolated and independently testable. Each module exposes
`fn try_rewrite_X(match_cmd, cmd_body) -> Option<String>` — a uniform interface with no shared
mutable state. `lazy_static!` regexes compile once per module.

### #241: RTK as a rewrite oracle

RTK does not touch the JSON protocol at all. The bash hook reads the JSON, extracts the
command string, calls `rtk rewrite "cmd"`, and reassembles the JSON output itself. The hook
is ~71 lines of bash. The Rust binary just prints the rewritten string and exits 0 (match)
or 1 (no match).

`src/rewrite_cmd.rs` is 48 lines and delegates entirely to `src/discover/registry.rs`, which
already existed for the `rtk discover` feature. The compound rewriting (`&&`, `||`, `;`, `|`,
`&`) lives in `registry::rewrite_compound()` as a byte-by-byte state machine.

---

## Capability Comparison

| Capability | [#156](https://github.com/rtk-ai/rtk/pull/156) | [#150](https://github.com/rtk-ai/rtk/pull/150) | [#241](https://github.com/rtk-ai/rtk/pull/241) | Why It Matters |
|------------|:---:|:---:|:---:|----------------|
| **Hook protocol** | | | | |
| RTK reads PreToolUse JSON from stdin | ✅ `claude_hook.rs` | ✅ `hook/mod.rs` | ❌ bash reads JSON | No bash/jq dependency; works natively on Windows |
| Compile-time stdout isolation (`#![deny(print_stdout)]`) | ✅ | ❌ | n/a | Prevents stray prints corrupting JSON protocol at compile time, not just by convention |
| Fail-open on all error paths (exit 0, no output) | ✅ | ✅ | ✅ `\|\| exit 0` | Any hook bug must leave the user unblocked |
| Bug #4669 workaround (exit 2 + stderr for deny) | ✅ `claude_hook.rs:220-222` | ❌ | ❌ | Claude Code ignores `"deny"` at exit 0; without this, blocks silently fail to block |
| **Command rewriting** | | | | |
| Quote-aware chain splitting | ✅ `lexer.rs` state machine | ✅ `hook/mod.rs` quote tracking | ✅ `rewrite_compound()` byte-by-byte | `git commit -m "Fix && Bug"` must not split on `&&` inside quotes |
| `&&` / `\|\|` / `;` chain rewriting | ✅ | ✅ | ✅ | Closes [#112](https://github.com/rtk-ai/rtk/issues/112): `cd /tmp && git status` previously only rewrote `cd` |
| Pipe handling | ✅ safe-suffix detection; general pipes run raw (preserves stream + downstream format correctness) | ❌ rewrites first segment; downstream tools (grep/awk) receive RTK-reformatted data, breaking pipe scripts | ❌ first segment only; same format-corruption risk | Scripts like `cargo test \| grep FAILED` depend on raw output format; rewriting corrupts grep's input |
| `&` background operator | ✅ **Fixed**: trailing `&` stripped as safe suffix in `split_safe_suffix()` when no Shellism in core (`cargo build &` → `rtk cargo build &` with savings); `cargo build 2>&1 &` → Shellism guard fires → shell passthrough; 4 regression tests added. | ✅ stripped from suffix, `cargo build` rewritten (~90% savings) | ⚠️ **Bug**: byte-by-byte parser (line 696) fires on ANY `&` outside quotes — no check for preceding `>`. `cargo build 2>&1` splits at `&` in `>&1`, producing `"rtk cargo build 2> & 1"` — broken redirect. `cargo build &` (simple) works. | `cargo build 2>&1` is extremely common; misidentifying `>&` as a background operator produces malformed rewrite |
| Env-prefix preservation (`TEST=1 git`) | ✅ | ✅ | ✅ | Env vars must survive the rewrite |
| Routes to specialized RTK filters | ✅ `registry.rs` | ✅ per-module whitelists | ✅ `registry.rs` | The routing regression (0% savings instead of 90%) was the first critical bug on both #156 and #150; now fixed in both |
| `git -C /path status` — safe skip | ✅ passthrough | ✅ explicit None | ✅ `rewrite_segment` None | Running in the wrong directory would be a silent data hazard |
| `gh --json`/`--jq`/`--template` — safe skip | ✅ | ✅ `git.rs:41-46` | ✅ registry | Structured data output must not be corrupted by RTK rewriting |
| `cat f1 f2` — multi-file handling | ✅ safe | ✅ explicit None | ⚠️ → `rtk read f1 f2` (may work) | `rtk read` behavior with multiple files needs verification |
| `head -20 file` → `rtk read file --max-lines 20` | ✅ | ✅ `files.rs:15-29` | ✅ `rewrite_head_numeric` | Otherwise `rtk read -20 file` is a clap parse error |
| `grep -r PATTERN .` flag reordering | ✅ | ✅ `files.rs:55-99` with value-consuming | ✅ registry | `rtk grep` clap expects pattern first; flags must be moved after `--` |
| **Execution** | | | | |
| RTK executes the rewritten command | ✅ `exec.rs` | ❌ Claude Code executes | ❌ Claude Code executes | Only RTK-owned execution enables streaming and redirect handling |
| Streaming output (line-by-line, not buffered) | ✅ `exec.rs` | ❌ | ❌ | `cargo build --release` takes 2+ min; buffering to completion produces no output during the build — UX regression flagged in [#156 review](https://github.com/rtk-ai/rtk/pull/156#issuecomment-3923337082) |
| `2>&1` / `> /dev/null` suffix stripping | ✅ `analysis.rs:split_safe_suffix()` | ❌ | ❌ | Common idioms; must not break routing |
| Builtin handling (`cd`, `export`, `pwd`, `echo`) | ✅ `builtins.rs` | ❌ | ❌ | `cd` changes the working directory for subsequent chained commands; cannot be executed as a child process |
| RAII recursion guard (`RTK_ACTIVE` env var) | ✅ clears on panic | ❌ not needed (pure rewriter) | ❌ | Prevents infinite recursion if `rtk run -c` rewrites to `rtk run -c` again |
| **Parallel-hook bug fix** | | | | |
| Patches plugin caches to make RTK the sole `Bash` hook | ✅ `rtk init` | ❌ | ❌ | Without this, any other plugin with a `Bash` matcher causes `updatedInput` to be silently dropped — see root cause section above |
| Manifest fallthrough (other plugins' rules still enforced) | ✅ `claude_hook.rs:265-330` | ❌ | ❌ | Without fallthrough, patching autorun's cache disables its `rm -rf` blocking and tool suggestions |
| Fallthrough: exit 2 only propagated if stdin write succeeded | ✅ `write_ok` guard | ❌ | ❌ | Prevents false denial when the handler pipe breaks before the handler can evaluate |
| Backup registry (`rtk-backups.json`) | ✅ | ❌ | ❌ | Idempotent `rtk init` re-runs; user can locate original files without filesystem searching |
| **Multi-LLM** | | | | |
| Gemini CLI hook (`rtk hook gemini`) | ✅ via [#158](https://github.com/rtk-ai/rtk/pull/158) | ❌ | ❌ | Gemini uses `BeforeTool`/`decision` vs Claude's `PreToolUse`/`permissionDecision`; shared `hook.rs` decision logic |
| Any LLM hook calls `rtk rewrite` as oracle | ❌ | ❌ | ✅ | Non-Rust hooks call `rtk rewrite "cmd"` and get the rewritten string back in ~15 lines of bash |
| **Platform** | | | | |
| Windows: no bash/jq required for hook | ✅ | ✅ | ❌ bash+jq required | Windows users need Git Bash or WSL for a bash hook |
| **Testing** | | | | |
| Unit tests passing | 814 | 524 | 455 | |
| Hook test lines / total (coverage ratio) | 2,662 / 3,693 **(72%)** | 757 / 1,537 (49%) | minimal — reuses `registry.rs` tests | Higher ratio = edge cases systematically covered; `registry.rs` has no tests for `2>&1` split behavior |
| Edge case tests for `&` / `2>&1` / `$VAR` / backticks | ✅ `test_background_job_is_shellism`, `test_compound_fd_redirect_2_to_1_has_shellism`, `test_simple_var_is_arg`, `test_backtick_substitution`, etc. | ❌ | ❌ | #241's `2>&1` bug would have been caught by a test like `assert_eq!(rewrite("cargo build 2>&1"), "rtk cargo build 2>&1")` |
| Real execution tested against actual toolchains | ✅ E2E tests added | ✅ 98/98 by pszymkowiak | ❌ | The routing regression (0% savings instead of 90%) was only caught by real execution testing |

---

## Technical Deep Dive

### #156: Key implementation details

**`src/cmd/claude_hook.rs`** — the JSON protocol handler
- Single I/O point: `run()` reads all stdin, calls `run_inner()` (pure logic), writes one
  response. The `#![deny(clippy::print_stdout)]` guard makes accidental protocol corruption
  a compile error.
- `HookResponse::NoOpinion` exits 0 with no stdout. This is what Claude Code expects when
  a hook has nothing to say — but it triggers manifest fallthrough first.
- Manifest fallthrough (`lines 265-330`): reads `rtk-bash-manifest.json`, spawns each
  registered handler via `sh -c`, writes the original payload to its stdin, inherits
  stdout/stderr. Exit code 2 is propagated only if stdin write succeeded (`write_ok` guard)
  — prevents a broken pipe from accidentally appearing as a deny.

**`src/lexer.rs`** — quote-aware tokenizer
- State machine with `quote: Option<char>`. Single quotes treat backslash as literal; double
  quotes interpret escapes. Unclosed quotes are treated as part of the arg (no panic).
- `$VAR` and `${VAR}` tokenize differently: simple identifiers → `Arg` (shell expands at
  execution time); `$()`, `${...}`, `$$`, `$?` → `Shellism` (forces shell passthrough).
  This means `git log $BRANCH` routes to `rtk git log $BRANCH` correctly.
- `2>/dev/null` tokenizes as `Arg("2")` + `Redirect(">")` + `Arg("/dev/null")` — the `2`
  becomes a separate token, which `split_safe_suffix()` later recognizes as the `2>&1`
  pattern.

**`src/cmd/analysis.rs`** — chain parsing
- `split_safe_suffix()` strips common trailing idioms (`2>&1`, `| tee file`, `| head N`,
  `2>/dev/null`, `>> file`) from the **end only**, checking that at least one core token
  remains. The stripped suffix is appended verbatim after the rewritten core command.
- `needs_shell()` returns true if any token is `Shellism`, `Pipe`, or `Redirect` (after
  suffix stripping). `Operator` (`&&`, `||`, `;`) does NOT trigger `needs_shell()` — chains
  are handled natively.
- `should_run()` implements `&&`/`||` short-circuit by tracking the **previous** operator
  and evaluating before each command: `&&` = run only if last exited 0; `||` = run only if
  last exited non-0.

**`src/cmd/exec.rs`** — execution pipeline
- `RtkActiveGuard` is RAII: sets `RTK_ACTIVE=1` on creation, clears it in `Drop` (runs even
  on panic). This prevents `rtk run -c "git status"` → rewrite → `rtk run -c "rtk git
  status"` → rewrite → infinite loop.
- Nested `rtk run -c` calls are flattened: `rtk run -c "rtk run -c 'git status'"` → `git
  status` directly (lines 88-99).
- State does **not** persist across hook invocations. Each `rtk hook claude` call is a new
  process; a `cd /tmp` in one call has no effect on the next.

**`src/cmd/builtins.rs`** — builtins
- `cd` calls `std::env::set_current_dir()` (changes the process's working directory for the
  duration of a single `rtk run -c` chain). `export` calls `std::env::set_var()`. Both
  affect child processes spawned within the same chain.
- Missing: identifier validation in `export` (accepts `123=x`).

### #150: Key implementation details

**`src/hook/mod.rs`** — orchestrator
- Dispatcher calls `try_rewrite_*()` on each module in order, returns first `Some()`.
  Each call is a `starts_with()` check — O(k) where k is command length, ~1µs total.
- `split_chain_segments()` (quote-aware) splits on ` && ` and ` ; `. Single-byte quote
  tracking. Each segment rewritten independently, rejoined with original operator.
- Env prefix regex: `^([A-Za-z_][A-Za-z0-9_]*=[^ ]* +)+` — captures leading `VAR=val`
  assignments, strips them, rewrites the core command, prepends them back.

**`src/hook/files.rs`** (318 lines, the most complex module)
- `reorder_grep_args()` (lines 55-99): separates flags from positionals, moves positionals
  first, appends flags after `--`. Consumes flag values using `GREP_FLAGS_WITH_VALUE`
  (`-A`, `-B`, `-C`, `-e`, `-f`, `-m`, etc.).
- `head -20 file` → `rtk read file --max-lines 20` via `HEAD_DASH_N_RE` regex.
  `head --lines=20 file` also handled. `head -c 100 file` returns None (falls through to
  generic `rtk read`).
- `cat f1 f2`: counts non-flag arguments; returns None if count ≠ 1 (multi-file not
  supported by `rtk read`). `cat -` (stdin) also returns None.

**`src/hook/git.rs`** (235 lines)
- `GIT_GLOBAL_FLAGS_RE` strips `-C /path` and other global flags **before** subcommand
  detection. After the P1 fix: `-C` returns None (no rewrite) rather than being stripped,
  preventing silent wrong-directory execution.
- Subcommand whitelist: `status`, `diff`, `log`, `add`, `commit`, `push`, `pull`,
  `branch`, `fetch`, `stash`, `show`, `worktree` — anything else returns None.

**Design:** Pure rewriter — no execution, no streaming, no builtins. Every function is
`fn try_rewrite_X(&str, &str) -> Option<String>` — stateless, pure, easily testable.

### #241: Key implementation details

**`src/rewrite_cmd.rs`** (48 lines)
- Thin wrapper: calls `registry::rewrite_command(cmd)`. Returns `Some` → print and exit 0.
  Returns `None` → `std::process::exit(1)`. The exit 1 is not an error — the bash hook uses
  `REWRITTEN=$(rtk rewrite "$CMD") || exit 0` to treat it as "no rewrite available."

**`src/discover/registry.rs`** (1496 lines, pre-existing for `rtk discover`)
- `rewrite_compound()` is a byte-by-byte state machine tracking `in_single`/`in_double`
  quote state. Handles `&&`, `||`, `;`, `|`, `&` as operators only outside quotes.
- Pipe (`|`) is special: only the first segment is rewritten; the rest is appended verbatim
  (`result.push_str(cmd[i..].trim_start())`). Rationale: preserve downstream consumers
  unchanged.
- Backtick command substitution (`` `cmd` ``) has **no special handling** — treated as
  normal text. `` git status `cat file` `` would attempt to rewrite normally.
- `$((` and `<<` are detected early and return None. `$()` subshells cause the whole
  segment to be skipped.
- `rewrite_segment()` handles `head -N file` → `rtk read file --max-lines N` via
  `HEAD_N` regex.
- `cat f1 f2` → `rtk read f1 f2` (not rejected — different from #150).

**`.claude/hooks/rtk-rewrite.sh`** (71 lines in the thin version)
- Requires both `rtk` and `jq` in PATH. Guards check for both; exits 0 silently if either
  is missing.
- Audit logging via `RTK_HOOK_AUDIT=1` → `~/.local/share/rtk/hook-audit.log`. Logs
  action (`rewrite`, `skip:no_match`, `skip:already_rtk`, `skip:heredoc`, etc.),
  original, and rewritten.
- Idempotency: compares `$CMD` to `$REWRITTEN`; exits 0 without output if unchanged.
- `include_str!("../hooks/rtk-rewrite.sh")` in `init.rs` now points to the 58-line thin hook at `hooks/rtk-rewrite.sh` (fixed). Earlier review found this embedded the old 218-line hook; it was corrected before the current worktree HEAD.

---

## Strengths and Weaknesses

### #156 (ahundt)

**Strengths**
- Only PR that fixes the parallel-hook `updatedInput` drop bug: patches plugin caches so RTK is the sole Bash hook; reads `rtk-bash-manifest.json` and spawns displaced handlers when it has no rewrite opinion, preserving autorun's safety rules
- Streaming execution (`exec.rs`): `cargo build --release` output flows line-by-line; no 5-minute silent wait before anything appears
- Safe-suffix pipe detection: `| tee build.log` and `| head -20` are stripped, RTK filters the core command, suffix is appended — these consumers are format-agnostic so it's safe to rewrite. General pipes (`| grep`) run raw so downstream tools receive unmodified data
- `write_ok` guard: exit 2 from a fallthrough handler is propagated only if stdin write succeeded — prevents a broken pipe from appearing as a deny that blocks the user
- Bug #4669 workaround: Claude Code silently ignores `"deny"` at exit 0; this PR uses exit 2 + stderr for actual blocks, which Claude Code does honor. Neither #150 nor #241 implement this
- Compile-time stdout isolation: `#![deny(clippy::print_stdout, clippy::print_stderr)]` in `claude_hook.rs` makes stray `println!()` a compile error, preventing the class of bug where a debug print corrupts the JSON protocol
- RAII recursion guard: `RtkActiveGuard` sets `RTK_ACTIVE=1` on creation, clears it in `Drop` even on panic — prevents `rtk run -c "git status"` → rewrite → infinite loop
- I/O spin bug fixed: `stream.rs` uses `.map_while(Result::ok)` instead of `.filter_map(Result::ok)` — stops on the first I/O error rather than spinning on a broken-pipe fd
- Windows-native: no bash or jq required
- Multi-LLM foundation: shared decision logic in `src/cmd/hook.rs`; #158 adds Gemini CLI support using the same infrastructure

- **Substantially more robust testing**: 2,662 test lines (72% of 3,693 total) vs 757 for #150 (49% of 1,537). Three core hook files alone (`lexer.rs` 674 lines, `analysis.rs` 675 lines, `claude_hook.rs` 591 lines) = 1,940 lines added. Specific tests cover every real-world edge case: `test_background_job_is_shellism()`, `test_compound_fd_redirect_2_to_1_has_shellism()`, `test_compound_fd_redirect_1_to_2_has_shellism()`, `test_combined_redirect_chain()`, `test_simple_var_is_arg()` (ensures `git log $BRANCH` routes natively), `test_dollar_subshell_stays_shellism()`, `test_quoted_operator_not_split()`, and more. The tests caught and prevented the exact classes of bugs found in #241 (`2>&1` misparse) and #150 (pipe segment rewriting).

**Weaknesses**
- Largest implementation: ~1,031 new implementation lines for hook logic (3,693 total including 2,662 test lines)
- Most complex execution model: `exec.rs`, `builtins.rs`, `lexer.rs`, `analysis.rs` all interact — harder to audit
- `cd /tmp && git status` in a chain: `cd` runs in-process and changes directory for that chain, but state does not persist to the next Claude Code tool invocation. The directory resets each time the hook is called
- Missing identifier validation in `export`: `export 123=x` is accepted silently
- pszymkowiak has not reviewed the post-routing-fix version; the earlier review found the routing regression (since fixed) but no re-review has happened

### #150 (ayoub-khemissi)

**Strengths**
- Modular 9-file design (`src/hook/{git,cargo,files,js_ts,containers,python,go,helpers}.rs`): each tool's rewriting logic is isolated and independently testable
- Pure rewriter — no execution engine means a smaller failure surface; a bug in the rewriter produces a wrong command string, not a crashed subprocess
- 4 review cycles; all reviewer-identified bugs fixed; 98/98 real-execution tests verified by pszymkowiak
- Implementation gap vs #156 is 1.3× (~780 vs ~1,031 lines), not the 3× implied by total diff size — most of the gap is tests
- `cat f1 f2` explicitly returns None (multi-file unsupported by `rtk read`) — #241 passes both files through which may or may not work
- `git -C /path status` explicitly returns None — no silent wrong-directory execution
- `grep` flag reordering with value-consuming flag handling (`-A 3`, `--include *.rs`, etc.) is thorough

**Weaknesses**
- Rewrites first segment of pipe chains, which breaks downstream tools: `cargo test | grep FAILED` becomes `rtk cargo test | grep FAILED`; grep receives RTK's reformatted failure summary, not raw `cargo test` output — the word "FAILED" may not appear in RTK's output at all, producing empty or wrong grep results
- No streaming: `rtk cargo build --release | tee build.log` buffers the entire 5-minute build internally before anything reaches tee — the pipe is there but it's not live
- Native Rust hook (`rtk hook-rewrite`) is opt-in via `--native-hook`; `rtk init -g` on Unix still installs the bash hook by default, preserving bash+jq dependency for most users
- No parallel-hook bug fix: rewrites silently fail with 0% savings for any user who has autorun or another Bash-matching plugin installed
- No Bug #4669 workaround: a `"deny"` response at exit 0 is silently ignored by Claude Code — blocks from this PR would appear to work but not actually block the command

### #241 (FlorianBruniaux)

**Strengths**
- Smallest new code surface: 48 lines of new Rust in `rewrite_cmd.rs`; rewriting logic reuses the existing `registry.rs` (already exercised by `rtk discover`)
- Oracle model enables lightweight integration: any LLM tool can call `rtk rewrite "cmd"` and get the rewritten string back — Claude Code integration is ~58 lines of bash, Gemini or other tools need ~15 lines
- `RTK_HOOK_AUDIT=1` built into the bash hook: logs action, original, and rewritten command to `~/.local/share/rtk/hook-audit.log` — useful for debugging without modifying the binary
- `&` simple trailing background operator: `cargo build &` → `rtk cargo build &` with savings. But `cargo build 2>&1` → `"rtk cargo build 2> & 1"` (broken — byte-by-byte parser splits on `&` in `2>&1`). AI agent use case works for simple cases only.
- All previously-identified review-blocking bugs are now fixed in the worktree (see status section below)

**Weaknesses**
- Requires both `rtk` and `jq` in PATH on every platform including Windows — Windows users need Git Bash or WSL
- Same pipe correctness problem as #150: `rewrite_compound()` rewrites only the first pipe segment; downstream consumers receive RTK-reformatted data, not raw output
- No streaming: long-running commands buffer before piped consumers see anything
- No parallel-hook bug fix: same 0%-savings failure as #150 for autorun users
- No Bug #4669 workaround
- Backtick command substitution (`` `cmd` ``) has no special handling — treated as normal text; `` git status `cat file` `` attempts rewrite normally, may produce malformed command
- **`2>&1` bug**: `rewrite_compound()` byte-by-byte parser (line 696 in `registry.rs`) splits on ANY `&` outside quotes, with no check for preceding `>`. `cargo build 2>&1` → `"rtk cargo build 2> & 1"` — the redirect is completely broken. `2>&1` is one of the most common shell patterns (redirecting stderr to stdout); this bug affects most real-world CI and script usage.
- No `write_ok` guard on safety-rule propagation

---

## Failure Mode Table

Each row shows a concrete input command, what each PR does with it, and the consequence for the user.

| Input command | #156 | #150 | #241 | Consequence |
|--------------|------|------|------|-------------|
| **Installation / environment** | | | | |
| `autorun` plugin installed, user runs `git status` | ✅ rewrites to `rtk git status` (cache patched) | ❌ rewrite silently dropped, runs as-is | ❌ rewrite silently dropped, runs as-is | #150/#241: 0% savings, no error message, no indication anything is wrong |
| `jq` not installed | ✅ unaffected | ✅ unaffected | ❌ hook exits 0 silently, original command runs unmodified | #241 on macOS without Homebrew jq: zero rewrites, zero errors |
| RTK binary is v0.22 (predates `hook-rewrite`/`rewrite`) | n/a | ❌ `rtk hook-rewrite` not found → hook exits 0 silently | ⚠️ version guard prints warning to stderr, falls through | #150 silent; #241 warns |
| `rtk init -g` fresh install on Unix | ✅ installs `rtk hook claude`, patches plugin caches | ⚠️ installs bash hook by default; `--native-hook` required for Rust hook | ✅ installs 58-line bash thin hook | #150 default: bash+jq dependency remains unless user passes flag |
| `rm -rf /` with autorun installed | ✅ safety rule preserved via manifest fallthrough | ❌ safety rule bypassed (autorun's Bash matcher still present; parallel hooks → updatedInput dropped AND autorun deny may not register) | ❌ same as #150 | #150/#241: autorun was supposed to block this, but parallel-hook drop means neither hook's response wins cleanly |
| Broken pipe reading subprocess output | ✅ `map_while` stops on first error | ✅ (no subprocess) | ✅ (no subprocess) | #156 fixed real hang bug; others not applicable |
| **Pipe and streaming** | | | | |
| `cargo test \| grep FAILED` | ✅ routes raw (grep gets real cargo output) | ❌ `rtk cargo test \| grep FAILED` — grep receives RTK's reformatted summary | ❌ first segment rewritten, grep receives reformatted data | #150/#241: grep may return empty or wrong results; script breaks silently |
| `cargo build --release \| tee build.log` (5-min build) | ✅ `\| tee` is safe suffix → streams live | ❌ RTK buffers entire build, dumps to tee after 5 min | ❌ same buffering, no live output | #150/#241: tee log incomplete until build finishes; UX regression for long builds |
| `cargo test \| head -20` | ✅ `\| head -20` is safe suffix → rewrites core | ❌ head receives reformatted RTK output | ❌ first segment rewritten, head gets reformatted data | #150/#241: head may see fewer lines than intended because RTK already condensed output |
| **Command routing correctness** | | | | |
| `git -C /other/repo status` | ✅ lexer routes to shell (safe passthrough) | ✅ explicit None → original runs | ✅ explicit None → original runs | All safe; different mechanism |
| `gh --json number pr list` | ✅ registry skips (structured output) | ✅ explicit None | ✅ registry skips | All safe |
| `cat file1 file2` | ✅ passthrough (safe) | ✅ explicit None | ⚠️ `rtk read file1 file2` — behavior depends on `rtk read` multi-file support | #241 may produce unexpected behavior on multi-file cat |
| `` git status `cat branches.txt` `` | ✅ backtick → Shellism → passthrough | ✅ no match → passthrough | ❌ backtick not detected → attempts rewrite → may produce malformed command | #241: `` `cat branches.txt` `` treated as normal args, could break routing |
| `git commit -m "fix && bug"` (quoted `&&`) | ✅ lexer tracks quotes, no split | ✅ quote-aware splitter, no split | ✅ byte-by-byte quote tracking, no split | All handle correctly |
| `cargo build &` (background) | ✅ **Fixed**: trailing `&` stripped as safe suffix → `rtk cargo build &` with savings. Guard: no other Shellism in core. `cargo build 2>&1 &` → Shellism guard → shell passthrough. | ✅ stripped from suffix, `cargo build` rewritten (~90% savings) | ✅ works (last `&`, no preceding `>`) | #241: correct for simple trailing `&` only; `2>&1` is still broken |
| `cargo build 2>&1` (fd redirect) | ✅ `&` in `2>&1` → Shellism → shell passthrough; `2>&1` handled correctly | ✅ no subprocess → Claude Code handles redirect | ❌ byte-by-byte parser splits on `&` in `2>&1` → produces `"rtk cargo build 2> & 1"` — malformed command | #241 bug: `2>&1` is one of the most common shell patterns; misparse corrupts the redirect |
| `RUST_LOG=debug cargo test` | ✅ env prefix preserved | ✅ env prefix regex strips and restores | ✅ env prefix handling | All handle correctly |
| **Safety / deny** | | | | |
| Autorun blocks `sed` (suggests Edit tool instead) | ✅ exit 2 + stderr propagated via manifest fallthrough | ❌ block silently ignored at exit 0 (Bug #4669) | ❌ block silently ignored at exit 0 (Bug #4669) | #150/#241: autorun's deny goes unheard; `sed` executes anyway |
| RTK itself wants to block a command | ✅ exit 2 + stderr → Claude Code honors block | ❌ exit 0 deny ignored by Claude Code | ❌ exit 0 deny ignored by Claude Code | #150/#241: RTK cannot actually block commands |
| **Infrastructure** | | | | |
| Plugin cache regenerated by `autorun update` | ⚠️ Bash matcher returns to cache; re-run `rtk init` to re-patch | n/a | n/a | #156 only: requires manual re-run after plugin updates |
| Manifest file missing or deleted | ✅ NoOpinion, exits 0 cleanly | n/a | n/a | Graceful degradation |
| `rtk` crashes mid-chain | ✅ RAII guard clears `RTK_ACTIVE`; partial side effects may persist | n/a | n/a | Chain state not fully atomic |

---

## Size

| PR | Lines added vs master | Hook implementation lines | Hook test lines | Core hook files |
|----|----------------------|--------------------------|-----------------|-----------------|
| **#156** | +9,441 | ~1,031 | ~2,662 | `lexer.rs` + `analysis.rs` + `exec.rs` + `claude_hook.rs` + `builtins.rs` + `hook.rs` |
| **#150** | +2,864 | ~780 | ~757 | `src/hook/` (9 files) |
| **#241** | +1,733 | ~48 (new) | minimal | `rewrite_cmd.rs` (reuses `registry.rs`) |

---

## Review-Blocking Bugs in #241 — Status

Identified by pszymkowiak [Mar 2](https://github.com/rtk-ai/rtk/pull/241#issuecomment-2692148567).
All three have since been fixed in the worktree:

| Bug | Status | Fix |
|-----|--------|-----|
| `rtk init` installs old 218-line hook | ✅ Fixed | `include_str!("../hooks/rtk-rewrite.sh")` now points to 58-line thin hook in `hooks/` (not `.claude/hooks/`) |
| `head -20 file` → clap crash | ✅ Fixed | `rewrite_head_numeric` translates to `rtk read file --max-lines N`; unsupported `head -c` flags return exit 1 |
| Silent failure with RTK < 0.23.0 | ✅ Fixed | Version guard added; prints warning to stderr instead of silently doing nothing |

Missing registry entries vs old hook were also added: `gh release`, `kubectl describe`/`apply`,
`docker run`/`exec`/`build`, `cargo install`, `tree`, `diff`.

---

## Additional Findings From Commit Messages and Diffs

### #150: Native hook is opt-in, not default

`rtk init -g` on Unix still installs the bash hook by default. The native Rust hook (`rtk hook-rewrite`) requires `rtk init -g --native-hook`. Rationale from the commit: "the native hook has 3 P0 bugs that need battle-testing before becoming the default." Windows gets native automatically (no bash available). Consequence: Unix users get the bash+jq dependency unless they explicitly opt in.

### #156: Real I/O spin bug fixed in `stream.rs`

`filter_map(Result::ok)` on `BufReader::lines()` silently skips I/O errors and can spin indefinitely on a broken-pipe fd (e.g., the read end closes while the child process is still writing). Changed to `.map_while(Result::ok)` at 5 sites — stops iteration on first error. Safe because pipes from `std::process::Child` are blocking fds: EINTR is retried internally, EAGAIN cannot occur. This is a real correctness fix, not a style change.

### #156: Dead test assertions (`bare matches!()`) were no-ops

Several tests in the hook logic used `matches!(expr, pattern)` without wrapping in `assert!()`. These compiled and "passed" but never actually validated anything. Fixed to `assert!(matches!(...))`. The fact that these existed shows the test suite had gaps that weren't caught by CI — the test count was inflated.

### #156: Missing dispatch table entries for `npm/npx/pnpm` and `go`

`cmd/filters.rs`'s `get_filter_mode()` was missing entries for npm, npx, pnpm, and go — commands that had dedicated filter modules but weren't wired into the execution dispatch. Fixed in a later commit. Also `pipe_cmd.rs`'s `resolve_filter()` was missing mypy, ruff, prettier, golangci-lint.

### #241: `&` operator handling — partially correct, with a real bug

The commit message states: "AI agents increasingly use `cmd1 & cmd2` for parallel execution." `rewrite_compound()` handles single `&` (after checking `&&`) as a background operator by splitting at that position and rewriting each segment.

**Bug confirmed in code (registry.rs:696)**: The match arm fires on ANY `&` outside quotes, with no check for a preceding `>`. This means `2>&1` is misidentified:
- `cargo build 2>&1` → parser splits at `&` in `>&1` → produces `"rtk cargo build 2> & 1"` — broken redirect
- `cmd1 & cmd2` (both segments after `&` are args, no `>` involved) → `rtk cmd1 & rtk cmd2` — probably correct
- `cargo build &` (trailing `&`, no preceding `>`) → `rtk cargo build & ` — correct

**Contrast with #156**: `&` is classified as `Shellism` by the lexer. For `2>&1`, the `&` is Shellism, so `needs_shell()=true` → shell passthrough → `2>&1` works correctly. For `cargo build &`, the `&` is also Shellism → shell passthrough → background job works correctly, 0% savings.

**Small fix possible for #156**: Add trailing `&` as a safe suffix in `split_safe_suffix()` when no other Shellism exists in the core tokens. This handles `cargo build &` for savings while correctly declining to strip when `2>&1` is also present (its `&` is already Shellism → core has Shellism → no-strip guard fires).

### #241: All P0 review-blocking bugs fixed (see section below)

The three bugs pszymkowiak flagged on Mar 2 are all fixed in the current worktree HEAD (`ce26dd5`). The PR is unblocked from a bug standpoint; it's awaiting re-review.

---

## Reviewer Quotes (verbatim)

**pszymkowiak on [#156](https://github.com/rtk-ai/rtk/pull/156#issuecomment-3923337082)** (Feb 18 — before routing fix):
> "the architecture is genuinely well thought out — the lexer, fail-open design, RAII guard,
> and deny(clippy::print_stdout) are all excellent."
> "The foundation here is solid. The lexer, chain parsing, and fail-open protocol handling are
> exactly what we need."

Original criticism: routing regression where all commands went through `rtk run -c` instead
of specialized filters → 0% savings (measured with data table). Fixed by Feb 22.
Since then: registry-based routing, streaming, suffix-aware pipe routing, and E2E tests added.
pszymkowiak's Mar 2 comment was only a rebase request — he has not reviewed any post-fix work.

**pszymkowiak on [#150](https://github.com/rtk-ai/rtk/pull/150#issuecomment-3930218186)** (Feb 20):
> Ran 98/98 real-execution tests across all ecosystems (Rust, Node.js, Python, Go, Git, GitHub
> CLI, Docker/K8s, Files, Network, Meta). "All RTK filters work perfectly with real command
> output. Zero regressions vs master."

aeppling (Feb 19): "Approved. Code is correct for me." (Before module split.)
Two bugs found Mar 2 — `git -C` silent wrong-directory rewrite; `grep -r` clap argument
ordering crash — both fixed same day. pszymkowiak has not reviewed the fixes.

**pszymkowiak on [#241](https://github.com/rtk-ai/rtk/pull/241#issuecomment-2692148567)** (Mar 2):
> "rtk rewrite as a single source of truth is exactly the right direction, and the Rust
> implementation of rewrite_command() is solid."

Followed by three review-blocking bugs listed above. FlorianBruniaux has not responded.

---

## Architectural Question

**Does RTK own the hook protocol, or does RTK act as a rewrite oracle?**

**Protocol-owner** (#156, #150): RTK reads the LLM's JSON from stdin and writes `updatedInput`
JSON to stdout. The hook entry is just `"command": "rtk hook claude"` — no bash logic.
Enables plugin cache patching (the only available fix for the parallel-hook bug). Works
natively on Windows. New LLMs need a new protocol adapter; #156 shares decision logic between
Claude and Gemini via `src/cmd/hook.rs`.

**Oracle** (#241): `rtk rewrite "cmd"` prints the rewritten string; bash owns the JSON
protocol. Any LLM integrates in ~15 lines of bash using the oracle. Still requires bash+jq on
all platforms including Windows. Does not fix the parallel-hook bug.

These are different integration styles and could coexist: `rtk hook claude` for Claude Code,
`rtk rewrite` as an oracle for bash-based hooks on other tools. The practical issue is that
only the protocol-owner style enables the plugin cache patching needed to fix the parallel-hook
`updatedInput` drop bug on master.

### What #156 has that neither #150 nor #241 have
- Streaming execution (line-by-line output for long-running commands)
- `2>&1` / `| tee` suffix stripping with correct routing of the core command
- Builtin handling (`cd`, `export` in chains)
- Plugin cache patching + manifest fallthrough (fixes the parallel-hook bug)
- `write_ok` guard preventing false denials on broken pipes
- Compile-time stdout isolation (`#![deny(clippy::print_stdout)]`)
- Unblocks Gemini multi-LLM support via [#158](https://github.com/rtk-ai/rtk/pull/158)

### What #150 has over #156
- 3× less code (1,528 vs 2,695 lines for hook-specific code)
- 4 review cycles; all reviewer-identified bugs fixed
- Modular 9-file structure: each tool's logic is fully isolated
- No execution engine to maintain (simpler failure surface)
- 98/98 real-execution test coverage verified by pszymkowiak

### What #241 has over both
- Smallest surface area (+48 lines net new; reuses existing registry)
- Bash hook is ~15 lines for any LLM tool
- `RTK_HOOK_AUDIT=1` audit logging built into the bash hook
- `registry.rs` already tested by `rtk discover` usage

---

## Worktree Locations

| PR | Worktree |
|----|----------|
| #156 | `.worktrees/pr-rust-hooks-v2` |
| #150 | `.worktrees/pr-native-hook-rewrite` |
| #241 | `.worktrees/pr-rtk-rewrite` |
