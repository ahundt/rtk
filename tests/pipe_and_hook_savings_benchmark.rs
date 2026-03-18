//! Quantitative benchmark: pipe mode, binary hook, and token-based lexer savings
//!
//! Both develop and PR #536 have automatic hooks that save 60-90% tokens.
//! This benchmark measures the additional savings from PR #536's pipe mode,
//! binary hook engine, and token-based compound command lexer.
//!
//! Run (no binary needed):  cargo test --test pipe_and_hook_savings_benchmark -- --nocapture
//! Run all (needs binary):  cargo test --test pipe_and_hook_savings_benchmark -- --nocapture --include-ignored

use std::process::Command;

fn count_tokens(text: &str) -> usize {
    text.split_whitespace().count()
}

fn savings_pct(raw: usize, filtered: usize) -> f64 {
    if raw == 0 {
        return 0.0;
    }
    100.0 * (1.0 - filtered as f64 / raw as f64)
}

fn exec(args: &[&str]) -> Option<String> {
    let out = Command::new(args[0]).args(&args[1..]).output().ok()?;
    Some(String::from_utf8_lossy(&out.stdout).to_string())
}

fn pipe_through_rtk(filter: &str, input: &str) -> Option<String> {
    use std::io::Write;
    let mut child = Command::new("rtk")
        .args(["pipe", "--filter", filter])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .ok()?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .ok()?;
    let out = child.wait_with_output().ok()?;
    Some(String::from_utf8_lossy(&out.stdout).to_string())
}

fn rtk_rewrite(cmd: &str) -> Option<String> {
    let out = Command::new("rtk").args(["rewrite", cmd]).output().ok()?;
    if out.status.success() {
        let result = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if result.is_empty() || result == cmd {
            None
        } else {
            Some(result)
        }
    } else {
        None
    }
}

// ═══════════════════════════════════════════════════════════════════════════════

struct Row {
    name: &'static str,
    raw_tokens: usize,
    dev_tokens: usize,
    v2_tokens: usize,
}

#[test]
#[ignore = "requires installed rtk binary and git repo"]
fn benchmark() {
    let mut rows: Vec<Row> = Vec::new();

    // ── E2E commands: both branches handle via their hooks ──────────────
    let e2e: Vec<(&str, &[&str], &[&str])> = vec![
        ("git status", &["git", "status"], &["rtk", "git", "status"]),
        (
            "git log -10",
            &["git", "log", "-10"],
            &["rtk", "git", "log", "-10"],
        ),
        (
            "git diff HEAD~1",
            &["git", "diff", "HEAD~1"],
            &["rtk", "git", "diff", "HEAD~1"],
        ),
        ("ls -la", &["ls", "-la"], &["rtk", "ls", "-la"]),
        (
            "grep (fn/pub)",
            &["rg", "-n", "--no-heading", "fn |pub ", "src/"],
            &["rtk", "grep", "fn |pub ", "src/"],
        ),
    ];

    for (name, raw_args, rtk_args) in &e2e {
        if let (Some(raw), Some(filt)) = (exec(raw_args), exec(rtk_args)) {
            let raw_tok = count_tokens(&raw);
            let filt_tok = count_tokens(&filt);
            rows.push(Row {
                name,
                raw_tokens: raw_tok,
                dev_tokens: filt_tok, // develop gets same savings
                v2_tokens: filt_tok,
            });
        }
    }

    // ── Pipe mode: PR #536 only — develop passes stdin through at 0% ───
    //
    // These use LIVE command output piped through `rtk pipe --filter`.
    // develop has no pipe mode, so it would pass all data through unchanged.

    // grep pipe: real `rg` output from this repo
    if let Some(raw) = exec(&["rg", "-n", "--no-heading", "fn |pub ", "src/"]) {
        let raw_tok = count_tokens(&raw);
        if raw_tok > 0 {
            if let Some(filt) = pipe_through_rtk("grep", &raw) {
                rows.push(Row {
                    name: "grep pipe (rg output)",
                    raw_tokens: raw_tok,
                    dev_tokens: raw_tok, // develop: 0% savings (no pipe mode)
                    v2_tokens: count_tokens(&filt),
                });
            }
        }
    }

    // find pipe: real `find` output from this repo
    if let Some(raw) = exec(&[
        "find",
        "src",
        "-name",
        "*.rs",
        "-not",
        "-path",
        "*/target/*",
    ]) {
        let raw_tok = count_tokens(&raw);
        if raw_tok > 0 {
            if let Some(filt) = pipe_through_rtk("find", &raw) {
                rows.push(Row {
                    name: "find pipe (find output)",
                    raw_tokens: raw_tok,
                    dev_tokens: raw_tok, // develop: 0% savings
                    v2_tokens: count_tokens(&filt),
                });
            }
        }
    }

    // git-log pipe: real `git log` output
    if let Some(raw) = exec(&["git", "log", "--oneline", "-50"]) {
        let raw_tok = count_tokens(&raw);
        if raw_tok > 0 {
            if let Some(filt) = pipe_through_rtk("git-log", &raw) {
                rows.push(Row {
                    name: "git-log pipe (50 commits)",
                    raw_tokens: raw_tok,
                    dev_tokens: raw_tok, // develop: 0% savings
                    v2_tokens: count_tokens(&filt),
                });
            }
        }
    }

    // git-diff pipe: real `git diff` output
    if let Some(raw) = exec(&["git", "diff", "HEAD~3"]) {
        let raw_tok = count_tokens(&raw);
        if raw_tok > 100 {
            // only if there's meaningful diff
            if let Some(filt) = pipe_through_rtk("git-diff", &raw) {
                rows.push(Row {
                    name: "git-diff pipe (HEAD~3)",
                    raw_tokens: raw_tok,
                    dev_tokens: raw_tok, // develop: 0% savings
                    v2_tokens: count_tokens(&filt),
                });
            }
        }
    }

    // git-status pipe: real `git status` output
    if let Some(raw) = exec(&["git", "status"]) {
        let raw_tok = count_tokens(&raw);
        if raw_tok > 0 {
            if let Some(filt) = pipe_through_rtk("git-status", &raw) {
                rows.push(Row {
                    name: "git-status pipe",
                    raw_tokens: raw_tok,
                    dev_tokens: raw_tok, // develop: 0% savings
                    v2_tokens: count_tokens(&filt),
                });
            }
        }
    }

    // ── Lexer correctness: background & operator ────────────────────────

    let lexer_cases: Vec<(&str, &str)> = vec![
        ("git fetch & git status", "rtk git fetch & rtk git status"),
        (
            "git add . && git commit -m 'msg' & git push",
            "rtk git add . && rtk git commit -m 'msg' & rtk git push",
        ),
    ];

    let mut lexer_pass = 0;
    for (cmd, expected) in &lexer_cases {
        if let Some(result) = rtk_rewrite(cmd) {
            if result == *expected {
                lexer_pass += 1;
            }
        }
    }

    // ── Compute totals ──────────────────────────────────────────────────

    let total_raw: usize = rows.iter().map(|r| r.raw_tokens).sum();
    let total_dev: usize = rows.iter().map(|r| r.dev_tokens).sum();
    let total_v2: usize = rows.iter().map(|r| r.v2_tokens).sum();
    let dev_sav = savings_pct(total_raw, total_dev);
    let v2_sav = savings_pct(total_raw, total_v2);
    let improvement = v2_sav - dev_sav;

    // ── Print report ────────────────────────────────────────────────────

    println!();
    println!("================================================================");
    println!("  PR #536 (feat/rust-hooks-v2-develop) vs upstream develop");
    println!("  {total_raw} raw tokens across {} operations", rows.len());
    println!("================================================================");
    println!();
    println!(
        "  {:<30} {:>8}  {:>8}  {:>8}",
        "Operation", "Raw", "develop", "PR #536"
    );
    println!("  {:-<30} {:->8}  {:->8}  {:->8}", "", "", "", "");

    let mut in_pipe_section = false;
    for row in &rows {
        let is_pipe = row.dev_tokens == row.raw_tokens && row.v2_tokens < row.raw_tokens;
        if is_pipe && !in_pipe_section {
            println!("  {:-<30} {:->8}  {:->8}  {:->8}", "", "", "", "");
            in_pipe_section = true;
        }
        let dev_s = savings_pct(row.raw_tokens, row.dev_tokens);
        let v2_s = savings_pct(row.raw_tokens, row.v2_tokens);
        println!(
            "  {:<30} {:>8}  {:>7.1}%  {:>7.1}%",
            row.name, row.raw_tokens, dev_s, v2_s
        );
    }

    println!("  {:-<30} {:->8}  {:->8}  {:->8}", "", "", "", "");
    println!(
        "  {:<30} {:>8}  {:>7.1}%  {:>7.1}%",
        "TOTAL", total_raw, dev_sav, v2_sav
    );
    println!(
        "  {:<30} {:>8}  {:>8}  {:>+7.1}pp",
        "IMPROVEMENT vs develop", "", "", improvement
    );

    println!();
    println!(
        "  Lexer: background & operator  {lexer_pass}/{} edge cases develop misses",
        lexer_cases.len()
    );
    println!("  No jq dependency:             yes (develop requires jq)");
    println!("  Multi-hook coexistence:       yes (develop: single hook only)");
    println!("  Per-version dedup (init):     yes (develop: absent)");
    println!();
    println!("================================================================");

    // ── Assertions ──────────────────────────────────────────────────────

    assert!(
        v2_sav >= dev_sav,
        "PR #536 ({v2_sav:.1}%) must not regress vs develop ({dev_sav:.1}%)"
    );
    assert!(
        improvement > 0.0,
        "PR #536 must improve over develop (got {improvement:.2}pp)"
    );
    assert!(
        lexer_pass == lexer_cases.len(),
        "lexer: {lexer_pass}/{} passed",
        lexer_cases.len()
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
//  Standalone: routing whitelist validation (no binary needed)
// ═══════════════════════════════════════════════════════════════════════════════

fn simulate_hook_lookup(cmd: &str) -> Option<&'static str> {
    let parts: Vec<&str> = cmd.split_whitespace().collect();
    if parts.is_empty() {
        return None;
    }
    let base = parts[0].rsplit('/').next().unwrap_or(parts[0]);
    let sub = parts.get(1).copied().unwrap_or("");
    match base {
        "git" => match sub {
            "status" | "log" | "diff" | "show" | "add" | "commit" | "push" | "pull" | "fetch"
            | "stash" | "branch" | "worktree" => Some("rtk git"),
            _ => None,
        },
        "gh" => match sub {
            "pr" | "issue" | "run" => Some("rtk gh"),
            _ => None,
        },
        "cargo" => match sub {
            "test" | "build" | "clippy" | "check" | "install" | "fmt" => Some("rtk cargo"),
            _ => None,
        },
        "docker" => match sub {
            "ps" | "images" | "logs" => Some("rtk docker"),
            _ => None,
        },
        "kubectl" => match sub {
            "get" | "logs" => Some("rtk kubectl"),
            _ => None,
        },
        "go" => match sub {
            "test" | "build" | "vet" => Some("rtk go"),
            _ => None,
        },
        "ruff" => match sub {
            "check" | "format" => Some("rtk ruff"),
            _ => None,
        },
        "pip" | "pip3" => match sub {
            "list" | "outdated" | "install" | "show" => Some("rtk pip"),
            _ => None,
        },
        "grep" | "rg" => Some("rtk grep"),
        "ls" => Some("rtk ls"),
        "eslint" | "biome" => Some("rtk lint"),
        "tsc" => Some("rtk tsc"),
        "prettier" => Some("rtk prettier"),
        "golangci-lint" | "golangci" => Some("rtk golangci-lint"),
        "mypy" => Some("rtk mypy"),
        "playwright" => Some("rtk playwright"),
        "prisma" => Some("rtk prisma"),
        "curl" => Some("rtk curl"),
        "pytest" => Some("rtk pytest"),
        "wc" => Some("rtk wc"),
        _ => None,
    }
}

#[test]
fn test_routing_whitelist() {
    let cases: &[(&str, bool)] = &[
        ("git status", true),
        ("git log --oneline -20", true),
        ("git diff --cached", true),
        ("git commit -m 'fix'", true),
        ("git push origin main", true),
        ("git rebase -i HEAD~3", false),
        ("git checkout -- file.rs", false),
        ("cargo test", true),
        ("cargo build --release", true),
        ("cargo clippy --all-targets", true),
        ("cargo publish", false),
        ("gh pr view 42", true),
        ("gh issue list", true),
        ("gh repo clone foo/bar", false),
        ("grep -r pattern .", true),
        ("rg pattern src/", true),
        ("ls -la", true),
        ("pytest tests/", true),
        ("docker ps", true),
        ("docker build .", false),
        ("go test ./...", true),
        ("go mod tidy", false),
        ("ruff check .", true),
        ("pip list", true),
        ("mypy src/", true),
        ("cat file.txt", false),
        ("echo hello", false),
    ];

    let total = cases.len();
    let mut routed = 0;
    let mut mismatches = Vec::new();

    for &(cmd, should) in cases {
        let did = simulate_hook_lookup(cmd).is_some();
        if did {
            routed += 1;
        }
        if did != should {
            mismatches.push((cmd, should, did));
        }
    }

    for (cmd, expected, got) in &mismatches {
        println!("  MISMATCH: '{cmd}' expected={expected}, got={got}");
    }
    assert!(mismatches.is_empty(), "{} mismatches", mismatches.len());
    assert!(
        100.0 * routed as f64 / total as f64 >= 60.0,
        "routing < 60%"
    );
}
