//! Lexer-backed suffix handling for shell output routing that can be safely
//! reattached after a command rewrite.

use super::lexer::{redirect_is_fd_dup_or_close, tokenize, ParsedToken, TokenKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RewriteSuffix<'a> {
    pub(super) core: &'a str,
    pub(super) suffix: &'a str,
}

/// Split trailing output routing while preserving the original text. Input
/// redirects stay in the core so callers reject them conservatively; the shared
/// lexer permission classifier decides whether an output target needs approval.
pub(super) fn split_rewrite_suffix(cmd: &str) -> RewriteSuffix<'_> {
    let tokens = tokenize(cmd);
    let mut boundary = tokens.len();
    let mut idx = tokens.len();

    while idx > 0 {
        let token = &tokens[idx - 1];
        match token.kind {
            TokenKind::Redirect => {
                let Some(consumes_target) = redirect_consumes_target(&tokens, idx - 1) else {
                    break;
                };
                if consumes_target {
                    break;
                }
                boundary = idx - 1;
                idx -= 1;
            }
            TokenKind::Arg if idx >= 2 && tokens[idx - 2].kind == TokenKind::Redirect => {
                let Some(consumes_target) = redirect_consumes_target(&tokens, idx - 2) else {
                    break;
                };
                if !consumes_target {
                    break;
                }
                boundary = idx - 2;
                idx -= 2;
            }
            _ => break,
        }
    }

    if boundary == 0 || boundary >= tokens.len() {
        return RewriteSuffix {
            core: cmd,
            suffix: "",
        };
    }

    let cut = tokens[boundary].offset;
    let core = cmd[..cut].trim_end();
    RewriteSuffix {
        core,
        suffix: &cmd[core.len()..],
    }
}

fn redirect_consumes_target(tokens: &[ParsedToken], idx: usize) -> Option<bool> {
    let value = tokens[idx].value.as_str();

    if value.starts_with('<') {
        return None;
    }
    if redirect_is_fd_dup_or_close(value) {
        return Some(false);
    }
    if !value.contains('>') {
        return None;
    }

    let target = tokens.get(idx + 1)?;
    (target.kind == TokenKind::Arg).then_some(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(cmd: &str) -> (&str, &str) {
        let result = split_rewrite_suffix(cmd);
        (result.core, result.suffix)
    }

    #[test]
    fn splits_file_target_output_redirect() {
        assert_eq!(
            split("git status > /tmp/status.log"),
            ("git status", " > /tmp/status.log")
        );
    }

    #[test]
    fn preserves_multiple_output_suffixes_in_original_order() {
        assert_eq!(
            split("cargo test >> /tmp/test.log 2>&1"),
            ("cargo test", " >> /tmp/test.log 2>&1")
        );
    }

    #[test]
    fn keeps_dev_null_and_fd_dup_suffixes() {
        assert_eq!(
            split("git status > /dev/null 2>&1"),
            ("git status", " > /dev/null 2>&1")
        );
        assert_eq!(split("git status 2>&1"), ("git status", " 2>&1"));
    }

    #[test]
    fn leaves_input_redirects_in_core_for_callers_to_reject() {
        assert_eq!(split("cat < /tmp/input"), ("cat < /tmp/input", ""));
    }

    #[test]
    fn does_not_strip_pipe_tails() {
        assert_eq!(
            split("cargo test | tail -50"),
            ("cargo test | tail -50", "")
        );
    }

    #[test]
    fn preserves_suffix_without_inserting_spaces() {
        assert_eq!(
            split("git status>/tmp/status.log"),
            ("git status", ">/tmp/status.log")
        );
    }
}
