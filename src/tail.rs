//! tail command - read end of files with token-optimized output
//!
//! Provides compact tail output by:
//! - Stripping ANSI escape codes
//! - Adding line numbers for context
//! - Showing skipped lines summary

use crate::tracking;
use anyhow::{Context, Result};
use std::process::{Command, Stdio};

pub fn run(args: &[String], verbose: u8) -> Result<()> {
    let timer = tracking::TimedExecution::start();

    // Build tail command with all user args
    let mut cmd = Command::new("tail");
    for arg in args {
        cmd.arg(arg);
    }
    // Inherit stdin to allow piped input
    cmd.stdin(Stdio::inherit());

    let output = cmd.output().context("Failed to run tail")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        eprint!("{}", stderr);
        std::process::exit(output.status.code().unwrap_or(1));
    }

    let raw = String::from_utf8_lossy(&output.stdout).to_string();
    let filtered = compact_tail(&raw);

    if verbose > 0 {
        eprintln!(
            "Chars: {} → {} ({}% reduction)",
            raw.len(),
            filtered.len(),
            if !raw.is_empty() {
                100 - (filtered.len() * 100 / raw.len())
            } else {
                0
            }
        );
    }

    print!("{}", filtered);
    timer.track(
        &format!("tail {}", args.join(" ")),
        "rtk tail",
        &raw,
        &filtered,
    );

    Ok(())
}

/// Compact tail output by stripping ANSI codes and adding context
fn compact_tail(raw: &str) -> String {
    // Strip ANSI escape codes
    let stripped = strip_ansi(raw);

    // Count lines
    let lines: Vec<&str> = stripped.lines().collect();
    let total_lines = lines.len();

    if total_lines == 0 {
        return String::new();
    }

    // For small outputs, just return as-is with line numbers
    if total_lines <= 50 {
        return add_line_numbers(&stripped);
    }

    // For larger outputs, show with line numbers
    // (we keep all lines since tail already limits the output)
    add_line_numbers(&stripped)
}

/// Strip ANSI escape codes from text
fn strip_ansi(s: &str) -> String {
    // Simple ANSI stripping - removes escape sequences
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip escape sequence
            if let Some(&next) = chars.peek() {
                if next == '[' {
                    chars.next(); // consume '['
                    // Skip until we hit a letter (the terminating character)
                    while let Some(&ch) = chars.peek() {
                        chars.next();
                        if ch.is_ascii_alphabetic() {
                            break;
                        }
                    }
                    continue;
                }
            }
        } else {
            result.push(c);
        }
    }
    result
}

/// Add line numbers to output
fn add_line_numbers(s: &str) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let max_width = lines.len().to_string().len();

    lines
        .iter()
        .enumerate()
        .map(|(i, line)| format!("{:>width$}  {}", i + 1, line, width = max_width))
        .collect::<Vec<_>>()
        .join("\n")
        + if s.ends_with('\n') { "\n" } else { "" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_ansi_basic() {
        let input = "\x1b[32mgreen text\x1b[0m normal";
        let output = strip_ansi(input);
        assert_eq!(output, "green text normal");
    }

    #[test]
    fn test_strip_ansi_colors() {
        let input = "\x1b[1;31;42mbold red on green\x1b[0m";
        let output = strip_ansi(input);
        assert_eq!(output, "bold red on green");
    }

    #[test]
    fn test_strip_ansi_no_codes() {
        let input = "plain text without codes";
        let output = strip_ansi(input);
        assert_eq!(output, input);
    }

    #[test]
    fn test_add_line_numbers_basic() {
        let input = "line1\nline2\nline3";
        let output = add_line_numbers(input);
        assert!(output.contains("1  line1"));
        assert!(output.contains("2  line2"));
        assert!(output.contains("3  line3"));
    }

    #[test]
    fn test_add_line_numbers_pads() {
        let input = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj";
        let output = add_line_numbers(input);
        assert!(output.contains(" 1  a"));
        assert!(output.contains("10  j"));
    }

    #[test]
    fn test_compact_tail_empty() {
        let input = "";
        let output = compact_tail(input);
        assert!(output.is_empty());
    }

    #[test]
    fn test_compact_tail_small() {
        let input = "line1\nline2\nline3";
        let output = compact_tail(input);
        assert!(output.contains("line1"));
        assert!(output.contains("line2"));
        assert!(output.contains("line3"));
    }

    #[test]
    fn test_compact_tail_strips_ansi() {
        let input = "\x1b[32mINFO\x1b[0m message\n\x1b[31mERROR\x1b[0m failed";
        let output = compact_tail(input);
        assert!(!output.contains("\x1b"));
        assert!(output.contains("INFO"));
        assert!(output.contains("ERROR"));
    }
}
