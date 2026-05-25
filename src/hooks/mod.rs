//! Hook installation and lifecycle management for AI coding agents.

pub mod constants;
pub mod hook_audit_cmd;
pub mod hook_check;
#[deny(clippy::print_stdout, clippy::print_stderr)]
pub mod hook_cmd;
pub mod init;
pub mod integrity;
pub mod permissions;
pub mod rewrite_cmd;
// Safety policy engine (opt-in via RTK_SAFE_COMMANDS / RTK_BLOCK_TOKEN_WASTE).
// The public surface is fully tested via #[cfg(test)] but only `check_raw`
// is currently wired into the live hook path (via permissions.rs::
// check_command_with_safety). The remaining `pub` items are deliberate hook
// points for follow-up integration once user-rule discovery lands.
#[allow(dead_code)]
pub mod safety;
pub mod trust;
pub mod verify_cmd;
