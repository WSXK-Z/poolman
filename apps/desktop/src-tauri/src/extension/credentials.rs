use std::collections::HashMap;
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderCredentials {
    pub token: Option<String>,
    pub settings_token: Option<String>,
    pub hif_headers: Option<HifHeaders>,
    pub api_template: Option<serde_json::Value>,
    pub updated_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HifHeaders {
    pub leim: Option<String>,
    pub dliq: Option<String>,
}

pub struct CredentialStore {
    store: std::sync::Mutex<HashMap<String, ProviderCredentials>>,
}

impl CredentialStore {
    pub fn new() -> Self {
        Self {
            store: std::sync::Mutex::new(HashMap::new()),
        }
    }

    pub fn upsert(&self, provider: &str, token: Option<String>, settings_token: Option<String>, hif_headers: Option<HifHeaders>, api_template: Option<serde_json::Value>) {
        let mut store = self.store.lock().unwrap();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        let entry = store.entry(provider.to_string()).or_insert_with(|| ProviderCredentials {
            token: None,
            settings_token: None,
            hif_headers: None,
            api_template: None,
            updated_at: now,
        });

        if let Some(t) = token {
            entry.token = Some(t);
        }
        if let Some(st) = settings_token {
            entry.settings_token = Some(st);
        }
        if let Some(h) = hif_headers {
            entry.hif_headers = Some(h);
        }
        if let Some(at) = api_template {
            entry.api_template = Some(at);
        }
        entry.updated_at = now;
    }

    #[allow(dead_code)]
    pub fn get(&self, provider: &str) -> Option<ProviderCredentials> {
        let store = self.store.lock().unwrap();
        store.get(provider).cloned()
    }

    pub fn get_token(&self, provider: &str) -> Option<String> {
        let store = self.store.lock().unwrap();
        store.get(provider).and_then(|c| c.token.clone())
    }

    pub fn get_settings_token(&self, provider: &str) -> Option<String> {
        let store = self.store.lock().unwrap();
        store.get(provider).and_then(|c| c.settings_token.clone())
    }

    #[allow(dead_code)]
    pub fn has_token(&self, provider: &str) -> bool {
        let store = self.store.lock().unwrap();
        store.get(provider).map_or(false, |c| c.token.is_some())
    }

    pub fn summary(&self) -> serde_json::Value {
        let store = self.store.lock().unwrap();
        let mut map = serde_json::Map::new();
        for (provider, creds) in store.iter() {
            map.insert(provider.clone(), serde_json::json!({
                "token_captured": creds.token.is_some(),
                "settings_token_captured": creds.settings_token.is_some(),
                "template_captured": creds.api_template.is_some(),
                "updated_at": creds.updated_at,
            }));
        }
        serde_json::Value::Object(map)
    }
}

impl Default for CredentialStore {
    fn default() -> Self {
        Self::new()
    }
}
