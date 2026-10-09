use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::RwLock;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagedProvider {
    pub id: String,
    pub name: String,
    pub connected: bool,
    pub authed: bool,
    pub token_captured: bool,
    pub enabled: bool,
}

pub struct ProviderControl {
    enabled: RwLock<HashMap<String, bool>>,
    path: PathBuf,
}

impl ProviderControl {
    pub fn new(data_dir: &str) -> Self {
        let path = PathBuf::from(data_dir).join("providers.json");
        let enabled = fs::read(&path)
            .ok()
            .and_then(|data| serde_json::from_slice(&data).ok())
            .unwrap_or_default();
        Self {
            enabled: RwLock::new(enabled),
            path,
        }
    }

    pub fn is_enabled(&self, provider: &str) -> bool {
        self.enabled
            .read()
            .map(|state| state.get(provider).copied().unwrap_or(true))
            .unwrap_or(true)
    }

    pub fn set_enabled(&self, provider: &str, value: bool) -> anyhow::Result<()> {
        let mut state = self
            .enabled
            .write()
            .map_err(|_| anyhow::anyhow!("供应商配置锁不可用"))?;
        let mut updated = state.clone();
        updated.insert(provider.to_string(), value);
        let data = serde_json::to_vec_pretty(&updated)?;
        fs::write(&self.path, data)?;
        *state = updated;
        Ok(())
    }

    pub fn list(&self, extension_status: &serde_json::Value) -> Vec<ManagedProvider> {
        const PROVIDERS: [(&str, &str); 6] = [
            ("deepseek-web", "DeepSeek"),
            ("chatgpt-web", "ChatGPT"),
            ("claude-web", "Claude"),
            ("gemini-web", "Gemini"),
            ("kimi-web", "Kimi"),
            ("qwen-web", "通义千问"),
        ];
        PROVIDERS
            .iter()
            .map(|(id, name)| {
                let status = extension_status
                    .get(*id)
                    .unwrap_or(&serde_json::Value::Null);
                ManagedProvider {
                    id: (*id).to_string(),
                    name: (*name).to_string(),
                    connected: status
                        .get("connected")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false),
                    authed: status
                        .get("authed")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false),
                    token_captured: status
                        .get("tokenCaptured")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false),
                    enabled: self.is_enabled(id),
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::ProviderControl;
    use std::fs;

    #[test]
    fn provider_enablement_persists_and_lists_extension_status() {
        let dir = std::env::temp_dir().join(format!("poolman-providers-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let dir_string = dir.to_string_lossy().to_string();

        let control = ProviderControl::new(&dir_string);
        control.set_enabled("chatgpt-web", false).unwrap();
        assert!(!control.is_enabled("chatgpt-web"));

        let restored = ProviderControl::new(&dir_string);
        assert!(!restored.is_enabled("chatgpt-web"));
        let providers = restored.list(&serde_json::json!({
            "chatgpt-web": {
                "connected": true,
                "authed": true,
                "tokenCaptured": true
            }
        }));
        let chatgpt = providers
            .iter()
            .find(|provider| provider.id == "chatgpt-web")
            .unwrap();
        assert!(chatgpt.connected);
        assert!(chatgpt.authed);
        assert!(chatgpt.token_captured);
        assert!(!chatgpt.enabled);

        fs::remove_file(dir.join("providers.json")).unwrap();
        fs::remove_dir(dir).unwrap();
    }
}
