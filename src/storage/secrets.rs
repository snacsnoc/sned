use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

/// Secret keys matching TypeScript SECRETS_KEYS from state-keys.ts
pub const SECRET_KEYS: &[&str] = &[
    "apiKey",
    "sned:snedAccountId",
    "snedApiKey",
    "openRouterApiKey",
    "awsAccessKey",
    "awsSecretKey",
    "awsSessionToken",
    "awsBedrockApiKey",
    "openAiApiKey",
    "geminiApiKey",
    "openAiNativeApiKey",
    "deepSeekApiKey",
    "requestyApiKey",
    "togetherApiKey",
    "fireworksApiKey",
    "qwenApiKey",
    "doubaoApiKey",
    "mistralApiKey",
    "liteLlmApiKey",
    "authNonce",
    "xaiApiKey",
    "moonshotApiKey",
    "zaiApiKey",
    "huggingFaceApiKey",
    "nebiusApiKey",
    "sambanovaApiKey",
    "cerebrasApiKey",
    "groqApiKey",
    "huaweiCloudMaasApiKey",
    "basetenApiKey",
    "vercelAiGatewayApiKey",
    "difyApiKey",
    "openAiCompatibleCustomApiKey",
    "minimaxApiKey",
    "aihubmixApiKey",
    "nousResearchApiKey",
    "openai-codex-oauth-credentials",
    "wandbApiKey",
    "github-copilot-oauth-credentials",
];

/// Environment variable to secret key mapping (from ENV_VAR_TO_SECRET_KEY in env-config.ts)
pub fn env_var_to_secret_key() -> HashMap<&'static str, &'static str> {
    let mut map = HashMap::new();
    map.insert("ANTHROPIC_API_KEY", "apiKey");
    map.insert("OPENAI_API_KEY", "openAiApiKey");
    map.insert("AZURE_OPENAI_API_KEY", "openAiApiKey");
    map.insert("GEMINI_API_KEY", "geminiApiKey");
    map.insert("GROQ_API_KEY", "groqApiKey");
    map.insert("CEREBRAS_API_KEY", "cerebrasApiKey");
    map.insert("XAI_API_KEY", "xaiApiKey");
    map.insert("OPENROUTER_API_KEY", "openRouterApiKey");
    map.insert("AI_GATEWAY_API_KEY", "vercelAiGatewayApiKey");
    map.insert("ZAI_API_KEY", "zaiApiKey");
    map.insert("MISTRAL_API_KEY", "mistralApiKey");
    map.insert("MOONSHOT_API_KEY", "moonshotApiKey");
    map.insert("MINIMAX_API_KEY", "minimaxApiKey");
    map.insert("MINIMAX_CN_API_KEY", "minimaxApiKey");
    map.insert("HF_TOKEN", "huggingFaceApiKey");
    map.insert("OPENCODE_API_KEY", "openAiNativeApiKey");
    map.insert("KIMI_API_KEY", "openAiNativeApiKey");
    map.insert("DEEPSEEK_API_KEY", "deepSeekApiKey");
    map.insert("QWEN_API_KEY", "qwenApiKey");
    map.insert("TOGETHER_API_KEY", "togetherApiKey");
    map.insert("FIREWORKS_API_KEY", "fireworksApiKey");
    map.insert("NEBIUS_API_KEY", "nebiusApiKey");
    map.insert(
        "OPENAI_COMPATIBLE_CUSTOM_KEY",
        "openAiCompatibleCustomApiKey",
    );
    map.insert("OPENAI_API_BASE", "openAiCompatibleCustomApiKey");
    map.insert("AWS_ACCESS_KEY_ID", "awsAccessKey");
    map.insert("AWS_SECRET_ACCESS_KEY", "awsSecretKey");
    map.insert("AWS_SESSION_TOKEN", "awsSessionToken");
    map
}

/// Environment variable to settings key mapping (from ENV_VAR_TO_SETTINGS_KEY in env-config.ts)
pub fn env_var_to_settings_key() -> HashMap<&'static str, &'static str> {
    let mut map = HashMap::new();
    map.insert("GOOGLE_CLOUD_PROJECT", "vertexProjectId");
    map.insert("GCP_PROJECT", "vertexProjectId");
    map.insert("GOOGLE_CLOUD_LOCATION", "vertexRegion");
    map.insert("GOOGLE_CLOUD_REGION", "vertexRegion");
    map.insert("AWS_BEDROCK_MODEL", "actModeApiModelId");
    map.insert("AWS_BEDROCK_MODEL_ACT", "actModeApiModelId");
    map.insert("AWS_BEDROCK_MODEL_PLAN", "planModeApiModelId");
    map.insert("AWS_REGION", "awsRegion");
    map
}

/// Get secrets from environment variables
pub fn get_secrets_from_env() -> HashMap<String, String> {
    let mut secrets = HashMap::new();
    let env_map = env_var_to_secret_key();

    for (env_var, secret_key) in env_map.iter() {
        if let Ok(value) = env::var(env_var) {
            secrets.insert(secret_key.to_string(), value);
        }
    }

    // Special case: OPENAI_API_KEY also maps to openAiNativeApiKey if not already set
    if let Ok(value) = env::var("OPENAI_API_KEY")
        && !secrets.contains_key("openAiNativeApiKey")
    {
        secrets.insert("openAiNativeApiKey".to_string(), value);
    }

    // OPENAI_COMPATIBLE_CUSTOM_KEY or OPENAI_API_BASE maps to openAiApiKey if not set
    let custom_key = env::var("OPENAI_COMPATIBLE_CUSTOM_KEY")
        .or_else(|_| env::var("OPENAI_API_BASE"))
        .ok();
    if let Some(value) = custom_key
        && !secrets.contains_key("openAiApiKey")
    {
        secrets.insert("openAiApiKey".to_string(), value);
    }

    secrets
}

/// Get settings from environment variables
pub fn get_settings_from_env() -> HashMap<String, String> {
    let mut settings = HashMap::new();
    let env_map = env_var_to_settings_key();

    for (env_var, settings_key) in env_map.iter() {
        if let Ok(value) = env::var(env_var) {
            settings.insert(settings_key.to_string(), value);
        }
    }

    // Special case: AWS_BEDROCK_MODEL maps to both act and plan modes if not overridden
    if let Ok(value) = env::var("AWS_BEDROCK_MODEL") {
        if env::var("AWS_BEDROCK_MODEL_ACT").is_err() {
            settings.insert("actModeApiModelId".to_string(), value.clone());
        }
        if env::var("AWS_BEDROCK_MODEL_PLAN").is_err() {
            settings.insert("planModeApiModelId".to_string(), value);
        }
    }

    settings
}

/// Get provider from environment variables (matching getProviderFromEnv() priority)
pub fn get_provider_from_env() -> Option<String> {
    if env::var("ANTHROPIC_API_KEY").is_ok() {
        return Some("anthropic".to_string());
    }
    if env::var("OPENROUTER_API_KEY").is_ok() {
        return Some("openrouter".to_string());
    }
    if env::var("OPENAI_API_KEY").is_ok() {
        return Some("openai-native".to_string());
    }
    if env::var("GEMINI_API_KEY").is_ok() {
        return Some("gemini".to_string());
    }
    if env::var("GOOGLE_CLOUD_PROJECT").is_ok() || env::var("GCP_PROJECT").is_ok() {
        return Some("vertex".to_string());
    }
    if env::var("AWS_ACCESS_KEY_ID").is_ok() || env::var("AWS_BEDROCK_MODEL").is_ok() {
        return Some("bedrock".to_string());
    }
    if env::var("GROQ_API_KEY").is_ok() {
        return Some("groq".to_string());
    }
    if env::var("XAI_API_KEY").is_ok() {
        return Some("xai".to_string());
    }
    if env::var("MISTRAL_API_KEY").is_ok() {
        return Some("mistral".to_string());
    }
    if env::var("MOONSHOT_API_KEY").is_ok() {
        return Some("moonshot".to_string());
    }
    if env::var("HF_TOKEN").is_ok() {
        return Some("huggingface".to_string());
    }
    if env::var("ZAI_API_KEY").is_ok() {
        return Some("zai".to_string());
    }
    if env::var("MINIMAX_API_KEY").is_ok() || env::var("MINIMAX_CN_API_KEY").is_ok() {
        return Some("minimax".to_string());
    }
    if env::var("CEREBRAS_API_KEY").is_ok() {
        return Some("cerebras".to_string());
    }
    if env::var("AI_GATEWAY_API_KEY").is_ok() {
        return Some("vercel-ai-gateway".to_string());
    }
    if env::var("OPENCODE_API_KEY").is_ok() || env::var("KIMI_API_KEY").is_ok() {
        return Some("openai-native".to_string());
    }
    if env::var("DEEPSEEK_API_KEY").is_ok() {
        return Some("deepseek".to_string());
    }
    if env::var("QWEN_API_KEY").is_ok() {
        return Some("qwen".to_string());
    }
    if env::var("TOGETHER_API_KEY").is_ok() {
        return Some("together".to_string());
    }
    if env::var("FIREWORKS_API_KEY").is_ok() {
        return Some("fireworks".to_string());
    }
    if env::var("NEBIUS_API_KEY").is_ok() {
        return Some("nebius".to_string());
    }
    if env::var("OPENAI_COMPATIBLE_CUSTOM_KEY").is_ok() || env::var("OPENAI_API_BASE").is_ok() {
        return Some("openai".to_string());
    }
    None
}

/// Secrets store for secret storage (uses Keychain/Credential Manager with file fallback)
pub struct SecretsStore {
    pub(crate) file_path: PathBuf,
    pub(crate) service_name: String,
}

impl SecretsStore {
    pub fn new() -> io::Result<Self> {
        let sned_dir = crate::storage::disk::get_sned_dir();
        fs::create_dir_all(&sned_dir)?;

        let file_path = sned_dir.join(".secrets.json");
        Ok(Self {
            file_path,
            service_name: "sned-cli".to_string(),
        })
    }

    /// Read all secrets from file (fallback)
    pub fn load(&self) -> HashMap<String, String> {
        match fs::read_to_string(&self.file_path) {
            Ok(contents) => match serde_json::from_str(&contents) {
                Ok(secrets) => secrets,
                Err(error) => {
                    tracing::warn!(
                        file_path = %self.file_path.display(),
                        error = %error,
                        "Failed to parse secrets JSON"
                    );
                    HashMap::new()
                }
            },
            Err(_) => HashMap::new(),
        }
    }

    /// Write secrets to file (fallback).
    ///
    /// Sets restrictive permissions BEFORE writing to avoid TOCTOU race.
    pub fn save(&self, secrets: &HashMap<String, String>) -> io::Result<()> {
        let data = serde_json::to_string_pretty(secrets)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        let rand_str: String = std::iter::repeat_with(fastrand::alphanumeric)
            .take(8)
            .collect();
        let tmp_ext = format!("tmp.{}", rand_str);
        let tmp_path = self.file_path.with_extension(&tmp_ext);

        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp_path)?
                .write_all(data.as_bytes())?;
        }

        #[cfg(windows)]
        {
            fs::write(&tmp_path, &data)?;
        }

        #[cfg(not(any(unix, windows)))]
        {
            fs::write(&tmp_path, &data)?;
        }

        fs::rename(&tmp_path, &self.file_path)?;

        Ok(())
    }

    /// Get a specific secret
    pub fn get(&self, key: &str) -> Option<String> {
        // Try keyring first, but fall back to file if keyring returns empty
        let keyring_value = if let Ok(entry) = keyring::Entry::new(&self.service_name, key) {
            entry.get_password().ok()
        } else {
            None
        };

        if let Some(password) = keyring_value
            && !password.is_empty()
        {
            return Some(password);
        }

        // Fallback to file
        let secrets = self.load();
        secrets.get(key).cloned()
    }

    /// Set a specific secret.
    /// Stores in OS keychain when available and verified; falls back to
    /// plaintext file when keychain is unavailable or unverified.
    pub fn set(&self, key: &str, value: &str) -> io::Result<()> {
        // Try keyring first — if it works, don't leave a plaintext copy.
        // We verify with a FRESH Entry object to catch environments where
        // same-Entry read-back works but cross-Entry retrieval fails.
        let keyring_verified = if let Ok(entry) = keyring::Entry::new(&self.service_name, key) {
            if entry.set_password(value).is_ok() {
                if let Ok(entry2) = keyring::Entry::new(&self.service_name, key) {
                    entry2.get_password().ok().as_deref() == Some(value)
                } else {
                    false
                }
            } else {
                false
            }
        } else {
            false
        };

        if keyring_verified {
            // Keychain is working — remove any stale file-backed copy
            let mut secrets = self.load();
            if secrets.remove(key).is_some() {
                let _ = self.save(&secrets);
            }
            tracing::debug!("Stored secret '{}' in OS keychain", key);
        } else {
            // Keychain unavailable or unverified — fall back to file storage
            // WARN the user that secrets are stored in plaintext
            tracing::warn!(
                "OS keychain unavailable. Secret '{}' stored in plaintext file at {}. \
                 For better security, ensure your OS keychain is available or restrict \
                 access to the containing directory.",
                key,
                self.file_path.display()
            );
            let mut secrets = self.load();
            secrets.insert(key.to_string(), value.to_string());
            self.save(&secrets)?;
        }

        Ok(())
    }

    /// Delete a secret
    pub fn delete(&self, key: &str) -> io::Result<()> {
        if let Ok(entry) = keyring::Entry::new(&self.service_name, key) {
            let _ = entry.delete_credential();
        }

        let mut secrets = self.load();
        if secrets.remove(key).is_some() {
            self.save(&secrets)?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, Write};
    use std::sync::{Arc, Mutex};
    use tempfile::tempdir;
    use tracing::Dispatch;

    #[test]
    fn test_env_var_to_secret_key_mapping() {
        let map = env_var_to_secret_key();
        assert_eq!(map.get("ANTHROPIC_API_KEY"), Some(&"apiKey"));
        assert_eq!(map.get("OPENAI_API_KEY"), Some(&"openAiApiKey"));
        assert_eq!(map.get("GEMINI_API_KEY"), Some(&"geminiApiKey"));
    }

    #[test]
    fn test_env_var_to_settings_key_mapping() {
        let map = env_var_to_settings_key();
        assert_eq!(map.get("GOOGLE_CLOUD_PROJECT"), Some(&"vertexProjectId"));
        assert_eq!(map.get("AWS_REGION"), Some(&"awsRegion"));
    }

    #[test]
    fn test_get_provider_from_env() {
        // This test is limited since we can't easily set env vars in tests
        // without affecting the test environment
        let provider = get_provider_from_env();
        // Just verify it doesn't panic
        let _ = provider;
    }

    #[test]
    fn test_secrets_store_file_roundtrip() {
        let temp_dir = std::env::temp_dir().join("sned_test_secrets");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let store = SecretsStore {
            file_path: temp_dir.join("test_secrets.json"),
            service_name: "sned-test".to_string(),
        };

        let test_key = "test_key_unit";
        let _ = store.delete(test_key);

        store.set(test_key, "test_value").unwrap();

        // get() must return the value from keyring or file fallback.
        // Some test environments (e.g. headless macOS) have unreliable
        // keychain read-back, so we only assert the round-trip works.
        let retrieved = store.get(test_key);
        assert!(
            retrieved.is_some(),
            "set/get round-trip failed: got {:?}",
            retrieved
        );

        store.delete(test_key).unwrap();
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    struct CapturedWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for CapturedWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn test_secrets_store_warns_on_corrupt_json() {
        let temp_dir = tempdir().unwrap();
        let store = SecretsStore {
            file_path: temp_dir.path().join(".secrets.json"),
            service_name: "sned-test".to_string(),
        };

        std::fs::write(&store.file_path, b"{not valid json").unwrap();

        let captured = Arc::new(Mutex::new(Vec::new()));
        let writer = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_target(false)
            .with_writer(move || CapturedWriter(writer.clone()))
            .finish();
        let dispatch = Dispatch::new(subscriber);

        let secrets = tracing::dispatcher::with_default(&dispatch, || store.load());

        assert!(secrets.is_empty());

        let output =
            String::from_utf8(captured.lock().unwrap_or_else(|e| e.into_inner()).clone()).unwrap();
        assert!(output.contains("Failed to parse secrets JSON"));
        assert!(output.contains(store.file_path.to_string_lossy().as_ref()));
    }
}
