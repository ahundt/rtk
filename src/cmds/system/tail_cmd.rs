//! `tail` command — compact end-of-file viewer.
//!
//! Wraps the system `tail` and post-processes its output by:
//! - Stripping ANSI escape codes (logs are often colorised)
//! - Prefixing each line with a numeric index (right-aligned, padded)
//! - Collapsing long bodies with a `... N lines skipped ...` marker so
//!   massive `tail` invocations still fit a reasonable token budget
//!
//! Donor: `feat/tail-command` `93ded41` (`src/tail.rs`), adapted for the
//! `src/cmds/system/` layout, the `core::runner::run_filtered` helper, and
//! the shared `core::utils::strip_ansi` regex.

use crate::core::runner::{self, RunOptions};
use crate::core::utils::{resolved_command, strip_ansi};
use anyhow::Result;

/// Threshold above which we collapse the middle of the output.
/// Tuned so a typical `tail -n 200` still prints in full but a pathological
/// `tail -n 10000 huge.log` produces a manageable summary.
const COLLAPSE_THRESHOLD: usize = 200;
/// When collapsing, keep this many lines from the head and tail of the output.
const KEEP_EDGE_LINES: usize = 50;

pub fn run(args: &[String], verbose: u8) -> Result<i32> {
    let mut cmd = resolved_command("tail");
    for arg in args {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: tail {}", args.join(" "));
    }

    // Tail reads from stdin when no file operands are supplied
    // (`cat foo.log | rtk tail -n 50`); forward our stdin so that works.
    let reads_stdin = !args.iter().any(|a| !a.starts_with('-'));
    let opts = if reads_stdin {
        RunOptions::stdout_only().inherit_stdin()
    } else {
        RunOptions::stdout_only()
    };

    runner::run_filtered(cmd, "tail", &args.join(" "), compact_tail, opts)
}

/// Compact `tail` output: strip ANSI, add line numbers, collapse if huge.
fn compact_tail(raw: &str) -> String {
    let stripped = strip_ansi(raw);
    let lines: Vec<&str> = stripped.lines().collect();

    if lines.is_empty() {
        return String::new();
    }

    let trailing_newline = stripped.ends_with('\n');

    let body = if lines.len() > COLLAPSE_THRESHOLD {
        collapse_middle(&lines)
    } else {
        add_line_numbers(&lines)
    };

    if trailing_newline {
        format!("{}\n", body)
    } else {
        body
    }
}

/// Right-pad line numbers to the width of the largest index so columns line up.
fn add_line_numbers(lines: &[&str]) -> String {
    let width = lines.len().to_string().len();
    lines
        .iter()
        .enumerate()
        .map(|(i, line)| format!("{:>width$}  {}", i + 1, line, width = width))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Keep first/last `KEEP_EDGE_LINES` and replace the middle with a summary line.
fn collapse_middle(lines: &[&str]) -> String {
    let total = lines.len();
    let width = total.to_string().len();
    let head_end = KEEP_EDGE_LINES;
    let tail_start = total - KEEP_EDGE_LINES;
    let skipped = tail_start - head_end;

    let mut out: Vec<String> = Vec::with_capacity(KEEP_EDGE_LINES * 2 + 1);

    for (i, line) in lines.iter().enumerate().take(head_end) {
        out.push(format!("{:>width$}  {}", i + 1, line, width = width));
    }

    out.push(format!(
        "{:>width$}  ... {} lines skipped ...",
        "",
        skipped,
        width = width
    ));

    for (offset, line) in lines.iter().enumerate().skip(tail_start) {
        out.push(format!("{:>width$}  {}", offset + 1, line, width = width));
    }

    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compact_tail_strips_ansi() {
        // ANSI-coloured log lines (the common case for `tail journalctl`-style output).
        let input = "\x1b[32mINFO\x1b[0m starting\n\x1b[31mERROR\x1b[0m failed\n";
        let output = compact_tail(input);
        assert!(!output.contains('\x1b'), "ANSI escape leaked: {output:?}");
        assert!(output.contains("INFO starting"));
        assert!(output.contains("ERROR failed"));
    }

    #[test]
    fn test_compact_tail_adds_line_numbers() {
        let input = "alpha\nbravo\ncharlie";
        let output = compact_tail(input);
        // Single-digit indices with no left-pad since max width = 1
        assert!(output.contains("1  alpha"), "missing '1  alpha' in {output:?}");
        assert!(output.contains("2  bravo"));
        assert!(output.contains("3  charlie"));
    }

    #[test]
    fn test_compact_tail_collapses_long_output() {
        // 300 lines > COLLAPSE_THRESHOLD (200) → middle is replaced by a summary marker.
        let lines: Vec<String> = (1..=300).map(|n| format!("line {n}")).collect();
        let input = lines.join("\n");
        let output = compact_tail(&input);

        // Summary marker present
        assert!(
            output.contains("... 200 lines skipped ..."),
            "expected skipped-lines marker, got:\n{output}"
        );
        // First and last edge lines survive
        assert!(output.contains("line 1"), "head missing");
        assert!(output.contains("line 300"), "tail missing");
        // A line firmly in the elided middle is gone
        assert!(
            !output.contains("line 150"),
            "middle should be collapsed, got:\n{output}"
        );
    }

    #[test]
    fn test_compact_tail_empty_input() {
        assert_eq!(compact_tail(""), "");
    }

    #[test]
    fn test_compact_tail_preserves_trailing_newline() {
        // `tail` output normally ends with `\n`; downstream pipes care about that.
        let input = "only line\n";
        let output = compact_tail(input);
        assert!(output.ends_with('\n'), "trailing newline lost: {output:?}");
        assert!(output.contains("1  only line"));
    }
}
