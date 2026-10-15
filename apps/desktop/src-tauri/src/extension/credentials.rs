//! 浏览器扩展捕获的供应商凭证及网页请求模板的进程内存储。
//!
//! 数据以供应商 ID 为键；凭证同步消息只更新实际提供的字段，
//! 因而后续只捕获到部分信息时不会清掉已保存的其它凭证字段。

use std::collections::HashMap;
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderCredentials {
    /// 网页访问接口使用的 bearer token 或同类凭证。
    pub token: Option<String>,
    /// DeepSeek 设置接口使用的独立 token。
    pub settings_token: Option<String>,
    /// DeepSeek 会话请求携带的网页专属校验头。
    pub hif_headers: Option<HifHeaders>,
    /// 内容脚本从真实网页请求捕获的 URL、请求头和字段模板。
    pub api_template: Option<serde_json::Value>,
    /// 最近一次字段更新的 Unix 毫秒时间戳。
    pub updated_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HifHeaders {
    /// DeepSeek `x-hif-leim` 请求头值。
    pub leim: Option<String>,
    /// DeepSeek `x-hif-dliq` 请求头值。
    pub dliq: Option<String>,
}

pub struct CredentialStore {
    /// 以供应商标识隔离各站点凭证；锁只保护短暂的内存读写。
    store: std::sync::Mutex<HashMap<String, ProviderCredentials>>,
}

impl CredentialStore {
    pub fn new() -> Self {
        Self {
            store: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// 合并供应商最新捕获字段；传入 `None` 表示该字段此次未更新。
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

    /// 生成不包含凭证原文的健康检查摘要。
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
