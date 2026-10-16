//! 桌面端渠道启停的运行期快照。
//!
//! 启停选择的持久化真相在模型行（[`crate::providers::manager::ProviderManager`]）：启动时按渠道
//! 汇总读入本结构，界面切换后写库并同步刷新，请求路由直接读这里的内存映射（同步）。
//! 实时在线与登录状态来自扩展上报，两类数据在列表输出时合并。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::RwLock;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagedProvider {
    /// 与共享协议一致的稳定供应商标识。
    pub id: String,
    pub name: String,
    /// 扩展报告的页面连接状态。
    pub connected: bool,
    /// 扩展报告的网页登录状态。
    pub authed: bool,
    /// 桌面进程是否已收到该站点访问凭证。
    pub token_captured: bool,
    /// 用户在桌面端保存的启用开关。
    pub enabled: bool,
}

pub struct ProviderControl {
    enabled: RwLock<HashMap<String, bool>>,
}

impl ProviderControl {
    /// 用启动时从数据库汇总出的渠道启停映射构造快照。
    pub fn new(enabled: HashMap<String, bool>) -> Self {
        Self {
            enabled: RwLock::new(enabled),
        }
    }

    pub fn is_enabled(&self, provider: &str) -> bool {
        self.enabled
            .read()
            .map(|state| state.get(provider).copied().unwrap_or(true))
            .unwrap_or(true)
    }

    /// 刷新某个渠道的启停快照（持久化由调用方写入数据库）。
    pub fn set_enabled(&self, provider: &str, value: bool) -> anyhow::Result<()> {
        let mut state = self
            .enabled
            .write()
            .map_err(|_| anyhow::anyhow!("供应商配置锁不可用"))?;
        state.insert(provider.to_string(), value);
        Ok(())
    }

    /// 合并固定供应商目录、扩展实时状态和桌面启用状态供界面展示。
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

/// 读取数据目录中的渠道启停选择文件（`providers.json`）：文件不存在或内容不可解析时返回 `None`。
pub fn read_enablement_file(data_dir: &str) -> Option<HashMap<String, bool>> {
    let path = PathBuf::from(data_dir).join("providers.json");
    let data = fs::read(path).ok()?;
    serde_json::from_slice(&data).ok()
}

#[cfg(test)]
mod tests {
    use super::ProviderControl;
    use std::collections::HashMap;

    #[test]
    fn provider_enablement_gates_and_lists_extension_status() {
        let control = ProviderControl::new(HashMap::from([("chatgpt-web".to_string(), false)]));
        assert!(!control.is_enabled("chatgpt-web"));
        // 未记录的渠道视为启用
        assert!(control.is_enabled("deepseek-web"));

        control.set_enabled("deepseek-web", false).unwrap();
        assert!(!control.is_enabled("deepseek-web"));

        let providers = control.list(&serde_json::json!({
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
    }
}
