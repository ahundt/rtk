//! `tail` command — compact end-of-file viewer.
//!
//! Wraps the system `tail` and post-processes its output by:
//! - Stripping ANSI escape codes (logs are often colorised)
//! - Prefixing each line with a numeric index (right-aligned, padded)
//! - Collapsing long bodies with a `... N lines skipped ...` marker so
//!   massive `tail` invocations still fit a reasonable token budget

use crate::core::runner::{self, RunOptions};
use crate::core::utils::{exit_code_from_status, resolved_command, strip_ansi};
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

    if is_follow_mode(args) {
        let status = cmd.status()?;
        return Ok(exit_code_from_status(&status, "tail"));
    }

    // Tail reads from stdin when no file operands are supplied
    // (`cat foo.log | rtk tail -n 50`); forward our stdin so that works.
    let opts = if reads_from_stdin(args) {
        RunOptions::stdout_only().inherit_stdin()
    } else {
        RunOptions::stdout_only()
    };

    runner::run_filtered(cmd, "tail", &args.join(" "), compact_tail, opts)
}

fn reads_from_stdin(args: &[String]) -> bool {
    !has_file_operand(args)
}

fn has_file_operand(args: &[String]) -> bool {
    let mut idx = 0;
    while idx < args.len() {
        let arg = args[idx].as_str();
        if arg == "--" {
            return idx + 1 < args.len();
        }
        if takes_separate_value(arg) {
            idx += 2;
            continue;
        }
        if arg.starts_with('-') && arg.len() > 1 {
            idx += 1;
            continue;
        }
        return true;
    }
    false
}

fn takes_separate_value(arg: &str) -> bool {
    matches!(
        arg,
        "-n" | "-c" | "--lines" | "--bytes" | "--pid" | "--sleep-interval" | "--max-unchanged-stats"
    )
}

/// Returns true when `tail` is in follow mode (`-f`, `-F`, or `--follow`).
///
/// Follow mode is an ongoing stream. Capturing it for post-processing would
/// wait forever before printing anything, so RTK runs it as raw passthrough.
fn is_follow_mode(args: &[String]) -> bool {
    for arg in args {
        if arg == "--" {
            break;
        }
        if arg == "--follow" || arg.starts_with("--follow=") {
            return true;
        }
        if let Some(shorts) = arg.strip_prefix('-') {
            if !shorts.is_empty()
                && !shorts.starts_with('-')
                && (shorts.contains('f') || shorts.contains('F'))
            {
                return true;
            }
        }
    }
    false
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
    fn compact_tail_formats_small_outputs_exactly() {
        let ten_line_input = (1..=10)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let ten_line_expected = (1..=10)
            .map(|n| format!("{n:>2}  line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let cases = [
            ("empty input", "", String::new()),
            (
                "single-digit line numbers",
                "alpha\nbravo\ncharlie",
                "1  alpha\n2  bravo\n3  charlie".to_string(),
            ),
            (
                "trailing newline preserved for downstream pipes",
                "only line\n",
                "1  only line\n".to_string(),
            ),
            (
                "ANSI-coloured log lines are stripped",
                "\x1b[32mINFO\x1b[0m starting\n\x1b[31mERROR\x1b[0m failed\n",
                "1  INFO starting\n2  ERROR failed\n".to_string(),
            ),
            ("two-digit line-number padding", &ten_line_input, ten_line_expected),
        ];

        for (name, input, expected) in cases {
            assert_eq!(compact_tail(input), expected, "{name}");
        }
    }

    #[test]
    fn compact_tail_collapses_long_output_with_context() {
        // 300 lines > COLLAPSE_THRESHOLD (200): keep edges and summarize the middle.
        let lines: Vec<String> = (1..=300).map(|n| format!("line {n}")).collect();
        let input = lines.join("\n");
        let output = compact_tail(&input);
        let numbered_lines: Vec<&str> = output.lines().collect();

        assert_eq!(
            numbered_lines.len(),
            KEEP_EDGE_LINES * 2 + 1,
            "collapsed output should keep head/tail context plus one marker:\n{output}"
        );
        assert!(
            output.contains("... 200 lines skipped ..."),
            "expected skipped-lines marker, got:\n{output}"
        );
        assert!(numbered_lines[0].ends_with("line 1"), "head missing");
        assert!(numbered_lines[49].ends_with("line 50"), "head edge missing");
        assert!(
            numbered_lines[51].ends_with("line 251"),
            "tail edge missing"
        );
        assert!(numbered_lines[100].ends_with("line 300"), "tail missing");
        assert!(
            !output.contains("line 150"),
            "middle should be collapsed, got:\n{output}"
        );
    }

    #[test]
    fn follow_mode_detects_streaming_flags() {
        for args in [
            vec!["-f"],
            vec!["-F"],
            vec!["-n", "20", "-f", "app.log"],
            vec!["-fn", "20", "app.log"],
            vec!["--follow", "app.log"],
            vec!["--follow=name", "app.log"],
        ] {
            let args = args.into_iter().map(str::to_string).collect::<Vec<_>>();
            assert!(is_follow_mode(&args), "{args:?} should stream raw");
        }
    }

    #[test]
    fn follow_mode_ignores_filenames_after_option_separator() {
        let args = ["--", "-f"].into_iter().map(str::to_string).collect::<Vec<_>>();
        assert!(
            !is_follow_mode(&args),
            "{args:?} should treat -f as a file operand"
        );
    }

    #[test]
    fn stdin_mode_accounts_for_option_values() {
        for args in [
            vec![],
            vec!["-n", "50"],
            vec!["--lines", "20"],
            vec!["-50"],
            vec!["--lines=20"],
        ] {
            let args = args.into_iter().map(str::to_string).collect::<Vec<_>>();
            assert!(reads_from_stdin(&args), "{args:?} should inherit stdin");
        }

        for args in [
            vec!["app.log"],
            vec!["-n", "50", "app.log"],
            vec!["--lines=20", "app.log"],
            vec!["--", "-f"],
        ] {
            let args = args.into_iter().map(str::to_string).collect::<Vec<_>>();
            assert!(
                !reads_from_stdin(&args),
                "{args:?} should use file operands"
            );
        }
    }
}
