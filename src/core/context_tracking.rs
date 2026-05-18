//! Context trackers for model usage and environment history.
//!
//! Ported from TypeScript `ModelContextTracker.ts` and `EnvironmentContextTracker.ts`.

use crate::storage::task_storage::TaskStorage;
use std::time::{SystemTime, UNIX_EPOCH};

/// Tracks model/provider/mode usage for a task.
#[derive(Debug, Clone)]
pub struct ModelContextTracker {
    task_id: String,
}

impl ModelContextTracker {
    pub fn new(task_id: &str) -> Self {
        Self {
            task_id: task_id.to_string(),
        }
    }

    /// Record model usage, avoiding duplicate consecutive entries.
    pub async fn record_model_usage(
        &self,
        api_provider_id: &str,
        model_id: &str,
        mode: &str,
    ) -> std::io::Result<()> {
        let storage = TaskStorage::new(&self.task_id)?;
        let mut metadata = storage.read_task_metadata();

        // Check if last entry is the same
        if let Some(last) = metadata.model_usage.last()
            && last.model_id == model_id
            && last.model_provider_id == api_provider_id
            && last.mode == mode
        {
            return Ok(());
        }

        metadata
            .model_usage
            .push(crate::storage::task_storage::ModelUsageEntry {
                ts: current_timestamp(),
                model_id: model_id.to_string(),
                model_provider_id: api_provider_id.to_string(),
                mode: mode.to_string(),
            });

        storage.write_task_metadata(&metadata)
    }
}

/// Tracks environment metadata for a task.
#[derive(Debug, Clone)]
pub struct EnvironmentContextTracker {
    task_id: String,
}

impl EnvironmentContextTracker {
    pub fn new(task_id: &str) -> Self {
        Self {
            task_id: task_id.to_string(),
        }
    }

    /// Record environment snapshot, avoiding duplicate consecutive entries.
    pub async fn record_environment(&self) -> std::io::Result<()> {
        let storage = TaskStorage::new(&self.task_id)?;
        let mut metadata = storage.read_task_metadata();

        let current_env = collect_environment_metadata();

        // Check if last entry is the same
        if let Some(last) = metadata.environment_history.last()
            && self.is_same_environment(last, &current_env)
        {
            return Ok(());
        }

        metadata.environment_history.push(current_env);
        storage.write_task_metadata(&metadata)
    }

    fn is_same_environment(
        &self,
        a: &crate::storage::task_storage::EnvironmentMetadataEntry,
        b: &crate::storage::task_storage::EnvironmentMetadataEntry,
    ) -> bool {
        a.os_name == b.os_name
            && a.os_version == b.os_version
            && a.os_arch == b.os_arch
            && a.host_name == b.host_name
            && a.host_version == b.host_version
            && a.sned_version == b.sned_version
    }
}

fn current_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn collect_environment_metadata() -> crate::storage::task_storage::EnvironmentMetadataEntry {
    use std::env;

    let host_name = env::var("HOSTNAME").unwrap_or_else(|_| "Unknown".to_string());
    let sned_version = env!("CARGO_PKG_VERSION").to_string();

    // Get OS info
    let os_name = env::consts::OS.to_string();
    let os_version = env::consts::OS.to_string(); // Simplified - could use uname for more detail
    let os_arch = env::consts::ARCH.to_string();

    crate::storage::task_storage::EnvironmentMetadataEntry {
        ts: current_timestamp(),
        os_name,
        os_version,
        os_arch,
        host_name,
        host_version: "Unknown".to_string(), // Would need system call for actual version
        sned_version,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_model_context_tracker_records_usage() {
        let temp = TempDir::new().unwrap();
        let task_id = format!("test-{}", std::process::id());

        // Create task directory
        let task_dir = temp.path().join(&task_id);
        fs::create_dir_all(&task_dir).unwrap();

        // Override SNED_DATA_DIR for this test
        let original_data_dir = std::env::var("SNED_DATA_DIR").ok();
        unsafe {
            std::env::set_var("SNED_DATA_DIR", temp.path().to_str().unwrap());
        }

        let tracker = ModelContextTracker::new(&task_id);
        tracker
            .record_model_usage("anthropic", "claude-sonnet-4-5", "act")
            .await
            .unwrap();

        // Verify metadata was written
        let storage = TaskStorage::new(&task_id).unwrap();
        let metadata = storage.read_task_metadata();
        assert_eq!(metadata.model_usage.len(), 1);
        assert_eq!(metadata.model_usage[0].model_id, "claude-sonnet-4-5");
        assert_eq!(metadata.model_usage[0].model_provider_id, "anthropic");
        assert_eq!(metadata.model_usage[0].mode, "act");

        // Restore original env var
        if let Some(val) = original_data_dir {
            unsafe {
                std::env::set_var("SNED_DATA_DIR", val);
            }
        } else {
            unsafe {
                std::env::remove_var("SNED_DATA_DIR");
            }
        }
    }
}
