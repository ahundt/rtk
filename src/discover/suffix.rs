//! Lexer-backed suffix handling for shell output routing that can be safely
//! reattached after a command rewrite.

use super::lexer::{redirect_is_fd_dup_or_close, tokenize, ParsedToken, TokenKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SuffixSafety {
    AutoAllow,
    AskOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RewriteSuffix<'a> {
    pub(super) core: &'a str,
    pub(super) suffix: &'a str,
    pub(super) safety: SuffixSafety,
}

/// Split trailing output routing while preserving the original text. File
/// targets can be rewritten but require approval; input redirects stay in the
/// core so callers reject them conservatively.
pub(super) fn split_rewrite_suffix(cmd: &str) -> RewriteSuffix<'_> {
    let tokens = tokenize(cmd);
    let mut boundary = tokens.len();
    let mut safety = SuffixSafety::AutoAllow;
    let mut idx = tokens.len();

    while idx > 0 {
        let token = &tokens[idx - 1];
        match token.kind {
            TokenKind::Redirect => {
                let Some(part) = classify_redirect(&tokens, idx - 1) else {
                    break;
                };
                if part.consumes_target {
                    break;
                }
                boundary = idx - 1;
                safety = safety.max(part.safety);
                idx -= 1;
            }
            TokenKind::Arg if idx >= 2 && tokens[idx - 2].kind == TokenKind::Redirect => {
                let Some(part) = classify_redirect(&tokens, idx - 2) else {
                    break;
                };
                if !part.consumes_target {
                    break;
                }
                boundary = idx - 2;
                safety = safety.max(part.safety);
                idx -= 2;
            }
            _ => break,
        }
    }

    if boundary == 0 || boundary >= tokens.len() {
        return RewriteSuffix {
            core: cmd,
            suffix: "",
            safety: SuffixSafety::AutoAllow,
        };
    }

    let cut = tokens[boundary].offset;
    let core = cmd[..cut].trim_end();
    RewriteSuffix {
        core,
        suffix: &cmd[core.len()..],
        safety,
    }
}

#[derive(Debug, Clone, Copy)]
struct RedirectPart {
    safety: SuffixSafety,
    consumes_target: bool,
}

fn classify_redirect(tokens: &[ParsedToken], idx: usize) -> Option<RedirectPart> {
    let value = tokens[idx].value.as_str();

    if value.starts_with('<') {
        return None;
    }
    if redirect_is_fd_dup_or_close(value) {
        return Some(RedirectPart {
            safety: SuffixSafety::AutoAllow,
            consumes_target: false,
        });
    }
    if !value.contains('>') {
        return None;
    }

    let target = tokens.get(idx + 1)?;
    if target.kind != TokenKind::Arg {
        return None;
    }
    Some(RedirectPart {
        safety: if target.value == "/dev/null" {
            SuffixSafety::AutoAllow
        } else {
            SuffixSafety::AskOnly
        },
        consumes_target: true,
    })
}

impl SuffixSafety {
    fn max(self, other: Self) -> Self {
        if matches!(self, Self::AskOnly) || matches!(other, Self::AskOnly) {
            Self::AskOnly
        } else {
            Self::AutoAllow
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(cmd: &str) -> (&str, &str, SuffixSafety) {
        let result = split_rewrite_suffix(cmd);
        (result.core, result.suffix, result.safety)
    }

    #[test]
    fn splits_file_target_output_redirect_as_ask_only() {
        assert_eq!(
            split("git status > /tmp/status.log"),
            ("git status", " > /tmp/status.log", SuffixSafety::AskOnly)
        );
    }

    #[test]
    fn preserves_multiple_output_suffixes_in_original_order() {
        assert_eq!(
            split("cargo test >> /tmp/test.log 2>&1"),
            (
                "cargo test",
                " >> /tmp/test.log 2>&1",
                SuffixSafety::AskOnly
            )
        );
    }

    #[test]
    fn keeps_dev_null_and_fd_dup_suffixes_auto_allow_safe() {
        assert_eq!(
            split("git status > /dev/null 2>&1"),
            ("git status", " > /dev/null 2>&1", SuffixSafety::AutoAllow)
        );
        assert_eq!(
            split("git status 2>&1"),
            ("git status", " 2>&1", SuffixSafety::AutoAllow)
        );
    }

    #[test]
    fn leaves_input_redirects_in_core_for_callers_to_reject() {
        assert_eq!(
            split("cat < /tmp/input"),
            ("cat < /tmp/input", "", SuffixSafety::AutoAllow)
        );
    }

    #[test]
    fn does_not_strip_pipe_tails() {
        assert_eq!(
            split("cargo test | tail -50"),
            ("cargo test | tail -50", "", SuffixSafety::AutoAllow)
        );
    }

    #[test]
    fn preserves_suffix_without_inserting_spaces() {
        assert_eq!(
            split("git status>/tmp/status.log"),
            ("git status", ">/tmp/status.log", SuffixSafety::AskOnly)
        );
    }
}
