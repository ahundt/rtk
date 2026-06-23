//! RTK_ACTIVE recursion guard.
//!
//! Sets the `RTK_ACTIVE=1` environment variable while RTK is processing a hook
//! invocation, so any child process that re-invokes `rtk hook` (for example,
//! through a chained or shelled-out command) sees the marker and skips further
//! rewriting. The guard is a RAII drop — the variable is cleared even if a
//! `panic!` unwinds through the call site, which prevents the env from
//! leaking to subsequent test cases or interactive use.

/// Returns `true` when `RTK_ACTIVE` is set in the current environment.
///
/// Treats *any* value (including `RTK_ACTIVE=0` and the empty string) as
/// "set" so the guard cannot be silently bypassed by a stale variable from
/// a parent shell. The hook entry points pair this with `is_hook_disabled`
/// which compares `RTK_HOOK_ENABLED` against the string `"0"` explicitly.
pub fn is_rtk_active() -> bool {
    std::env::var("RTK_ACTIVE").is_ok()
}

/// RAII guard that sets `RTK_ACTIVE=1` on construction and clears it on drop.
///
/// Drop runs during stack unwinding from a `panic!`, so the variable is
/// always cleared. The struct is intentionally zero-sized so the guard
/// adds no runtime allocation overhead.
pub struct RtkActiveGuard;

impl RtkActiveGuard {
    pub fn new() -> Self {
        std::env::set_var("RTK_ACTIVE", "1");
        Self
    }
}

impl Default for RtkActiveGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for RtkActiveGuard {
    fn drop(&mut self) {
        std::env::remove_var("RTK_ACTIVE");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    /// Serialize env-var-mutating tests so parallel runners cannot race each other.
    fn env_lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// Returned by every env-touching test; resets the variable on drop
    /// even when a previous test panics and leaves it dirty.
    struct EnvReset {
        _lock: MutexGuard<'static, ()>,
    }
    impl EnvReset {
        fn new() -> Self {
            let lock = env_lock();
            std::env::remove_var("RTK_ACTIVE");
            Self { _lock: lock }
        }
    }
    impl Drop for EnvReset {
        fn drop(&mut self) {
            std::env::remove_var("RTK_ACTIVE");
        }
    }

    #[test]
    fn test_is_rtk_active_false_when_unset() {
        let _r = EnvReset::new();
        assert!(!is_rtk_active());
    }

    #[test]
    fn test_guard_sets_and_clears_rtk_active() {
        let _r = EnvReset::new();
        assert!(!is_rtk_active(), "precondition: RTK_ACTIVE must be unset");
        {
            let _g = RtkActiveGuard::new();
            assert!(is_rtk_active(), "guard must set RTK_ACTIVE on new()");
            assert_eq!(std::env::var("RTK_ACTIVE").unwrap(), "1");
        }
        assert!(!is_rtk_active(), "guard must clear RTK_ACTIVE on drop");
    }

    #[test]
    fn test_guard_clears_even_when_explicit_other_value_present() {
        // A previous run might have set RTK_ACTIVE to something unusual.
        // The guard always sets it to "1" and always removes it on drop.
        let _r = EnvReset::new();
        std::env::set_var("RTK_ACTIVE", "previous-value");
        assert!(is_rtk_active());
        {
            let _g = RtkActiveGuard::new();
            assert_eq!(std::env::var("RTK_ACTIVE").unwrap(), "1");
        }
        assert!(!is_rtk_active());
    }

    #[test]
    fn test_is_rtk_active_true_for_zero_value() {
        // RTK_ACTIVE=0 is still "set" — treated as active to prevent
        // accidental bypass when an outer shell exports the var with any value.
        let _r = EnvReset::new();
        std::env::set_var("RTK_ACTIVE", "0");
        assert!(is_rtk_active());
    }
}
