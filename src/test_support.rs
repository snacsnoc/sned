use std::sync::{Mutex, OnceLock};

/// Shared test-only lock for process-global environment mutations.
pub fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}
