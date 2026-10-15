//! 插件实例管理器：登记每台插件的身份与连接信息、维护在线连接与供应商状态、保存启用选择。
//!
//! 桥接层 [`crate::extension::websocket::WebSocketBridge`] 是插件唯一的连接通道，
//! 每台插件实例都由本管理器统一登记，因而「有哪些插件连接过、何时连接、启用与否」
//! 不会随进程退出丢失：可持久化部分写入数据目录下的 `plugins.json`。
//!
//! 可持久化：插件身份（id / 名称 / 版本 / 平台）、首末次连接时间、连接次数、
//! 插件级启用开关、「插件:供应商」级启用开关。
//! 仅内存：在线集合与最近上报的供应商状态。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::RwLock;
use std::time::{SystemTime, UNIX_EPOCH};

/// 插件在 WebSocket 握手时自报的身份信息（取自握手 URL 的查询参数）。
#[derive(Debug, Clone, Default)]
pub struct ClientInfo {
    /// 插件自报的稳定标识；为空时本次连接使用临时标识且不落库。
    pub client_id: Option<String>,
    /// 插件名称。
    pub name: Option<String>,
    /// 插件版本。
    pub version: Option<String>,
    /// 插件运行平台标识。
    pub platform: Option<String>,
}

/// 一台插件的持久化信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginInstance {
    pub id: String,
    pub name: String,
    pub version: String,
    pub platform: String,
    /// 首次连接时间（Unix 毫秒）。
    pub first_seen_at: u64,
    /// 最近一次连接时间（Unix 毫秒）。
    pub last_seen_at: u64,
    /// 累计连接次数。
    pub connect_count: u64,
    /// 插件级启用开关。
    pub enabled: bool,
    /// 「渠道 → 是否启用」映射。
    pub channels: HashMap<String, bool>,
}

impl PluginInstance {
    fn new(id: &str, now: u64) -> Self {
        Self {
            id: id.to_string(),
            name: String::new(),
            version: String::new(),
            platform: String::new(),
            first_seen_at: now,
            last_seen_at: now,
            connect_count: 0,
            enabled: true,
            channels: HashMap::new(),
        }
    }
}

/// 调度层组装候选所需的一次性快照（整体克隆，避免跨 await 持锁）。
#[derive(Debug, Default)]
pub struct RegistrySnapshot {
    /// 在线插件 id → 该插件上报的供应商状态。
    pub statuses: HashMap<String, serde_json::Value>,
    /// 插件 id → 插件级启用开关。
    pub enabled: HashMap<String, bool>,
    /// 「插件 id:供应商」→ 供应商级启用开关。
    pub provider_enabled: HashMap<String, bool>,
}

#[derive(Default)]
struct Runtime {
    /// 在线插件 id → 连接建立时间（Unix 毫秒）。
    connected: HashMap<String, u64>,
    /// 在线插件 id → 最近上报的供应商状态。
    statuses: HashMap<String, serde_json::Value>,
    /// 最近一次上报的供应商状态（任意插件），供外部快速读取。
    latest_status: serde_json::Value,
}

/// 插件实例管理器。
pub struct ExtensionManager {
    /// 插件注册表（持久化）。
    plugins: RwLock<HashMap<String, PluginInstance>>,
    /// 在线集合与实时状态（仅内存）。
    runtime: RwLock<Runtime>,
    /// 持久化文件路径。
    path: PathBuf,
}

impl ExtensionManager {
    pub fn new(data_dir: &str) -> Self {
        let path = PathBuf::from(data_dir).join("plugins.json");
        let plugins = fs::read(&path)
            .ok()
            .and_then(|data| serde_json::from_slice(&data).ok())
            .unwrap_or_default();
        Self {
            plugins: RwLock::new(plugins),
            runtime: RwLock::new(Runtime {
                latest_status: serde_json::json!({}),
                ..Default::default()
            }),
            path,
        }
    }

    /// 登记一条插件连接。自报 `client_id` 时复用同一实例（保留启用选择并累加连接次数），
    /// 否则本次连接使用临时标识且不落库。返回本次连接的插件 id。
    pub fn register(&self, client: &ClientInfo) -> String {
        let now = now_ms();
        let client_id = non_empty(client.client_id.as_deref());
        let id = client_id
            .clone()
            .unwrap_or_else(|| format!("ephemeral-{}", uuid::Uuid::new_v4()));

        if client_id.is_some() {
            let outcome = self.mutate(|plugins| {
                let entry = plugins
                    .entry(id.clone())
                    .or_insert_with(|| PluginInstance::new(&id, now));
                if let Some(name) = non_empty(client.name.as_deref()) {
                    entry.name = name;
                }
                if let Some(version) = non_empty(client.version.as_deref()) {
                    entry.version = version;
                }
                if let Some(platform) = non_empty(client.platform.as_deref()) {
                    entry.platform = platform;
                }
                entry.last_seen_at = now;
                entry.connect_count += 1;
            });
            if let Err(e) = outcome {
                tracing::warn!("插件连接信息落库失败: {}", e);
            }
        }

        if let Ok(mut runtime) = self.runtime.write() {
            runtime.connected.insert(id.clone(), now);
            runtime.statuses.insert(id.clone(), serde_json::json!({}));
        }
        id
    }

    /// 标记插件连接断开：移出在线集合与状态表，保留其持久化注册信息。
    pub fn disconnect(&self, id: &str) {
        if let Ok(mut runtime) = self.runtime.write() {
            runtime.connected.remove(id);
            runtime.statuses.remove(id);
        }
    }

    /// 记录插件上报的供应商状态，同时作为最近状态。
    pub fn update_status(&self, id: &str, status: serde_json::Value) {
        if let Ok(mut runtime) = self.runtime.write() {
            runtime.latest_status = status.clone();
            runtime.statuses.insert(id.to_string(), status);
        }
    }

    /// 最近一次上报的供应商状态。
    pub fn latest_status(&self) -> serde_json::Value {
        self.runtime
            .read()
            .map(|runtime| runtime.latest_status.clone())
            .unwrap_or_else(|_| serde_json::json!({}))
    }

    /// 指定插件最近上报的供应商状态。
    pub fn status_of(&self, id: &str) -> Option<serde_json::Value> {
        self.runtime
            .read()
            .ok()
            .and_then(|runtime| runtime.statuses.get(id).cloned())
    }

    /// 是否有插件在线。
    #[allow(dead_code)]
    pub fn any_connected(&self) -> bool {
        self.runtime
            .read()
            .map(|runtime| !runtime.connected.is_empty())
            .unwrap_or(false)
    }

    /// 调度层组装候选所需的一次性快照。
    pub fn snapshot(&self) -> RegistrySnapshot {
        let statuses = self
            .runtime
            .read()
            .map(|runtime| runtime.statuses.clone())
            .unwrap_or_default();
        let (enabled, provider_enabled) = self
            .plugins
            .read()
            .map(|plugins| {
                let enabled = plugins
                    .iter()
                    .map(|(id, plugin)| (id.clone(), plugin.enabled))
                    .collect();
                let provider_enabled = plugins
                    .iter()
                    .flat_map(|(id, plugin)| {
                        plugin
                            .channels
                            .iter()
                            .map(move |(channel, on)| (format!("{id}:{channel}"), *on))
                    })
                    .collect();
                (enabled, provider_enabled)
            })
            .unwrap_or_default();
        RegistrySnapshot {
            statuses,
            enabled,
            provider_enabled,
        }
    }

    /// 供界面展示的插件列表：在线优先，其次按最近连接时间倒序。
    pub fn list(&self) -> Vec<serde_json::Value> {
        let connected = self
            .runtime
            .read()
            .map(|runtime| runtime.connected.clone())
            .unwrap_or_default();
        let statuses = self
            .runtime
            .read()
            .map(|runtime| runtime.statuses.clone())
            .unwrap_or_default();
        let plugins = self
            .plugins
            .read()
            .map(|plugins| plugins.clone())
            .unwrap_or_default();

        let mut items: Vec<serde_json::Value> = plugins
            .iter()
            .map(|(id, plugin)| {
                let channel_states = merge_channel_states(statuses.get(id), plugin);
                serde_json::json!({
                    "id": id,
                    "name": plugin.name,
                    "version": plugin.version,
                    "platform": plugin.platform,
                    "enabled": plugin.enabled,
                    "connected": connected.contains_key(id),
                    "firstSeenAt": plugin.first_seen_at,
                    "lastSeenAt": plugin.last_seen_at,
                    "connectCount": plugin.connect_count,
                    "channels": channel_states,
                })
            })
            .collect();

        // 未自报身份连接过、因而未落库的插件：只在此次运行期内可见。
        for (id, connected_at) in connected.iter() {
            if plugins.contains_key(id) {
                continue;
            }
            items.push(serde_json::json!({
                "id": id,
                "name": "",
                "version": "",
                "platform": "",
                "enabled": true,
                "connected": true,
                "firstSeenAt": connected_at,
                "lastSeenAt": connected_at,
                "connectCount": 1,
                "channels": statuses.get(id).cloned().unwrap_or_else(|| serde_json::json!({})),
            }));
        }

        items.sort_by(|a, b| {
            let a_connected = a.get("connected").and_then(serde_json::Value::as_bool).unwrap_or(false);
            let b_connected = b.get("connected").and_then(serde_json::Value::as_bool).unwrap_or(false);
            b_connected.cmp(&a_connected).then_with(|| {
                let a_seen = a.get("lastSeenAt").and_then(serde_json::Value::as_u64).unwrap_or(0);
                let b_seen = b.get("lastSeenAt").and_then(serde_json::Value::as_u64).unwrap_or(0);
                b_seen.cmp(&a_seen)
            })
        });
        items
    }

    /// 保存插件级启用开关。
    pub fn set_enabled(&self, id: &str, value: bool) -> anyhow::Result<()> {
        self.mutate(|plugins| {
            plugins
                .entry(id.to_string())
                .or_insert_with(|| PluginInstance::new(id, now_ms()))
                .enabled = value;
        })
    }

    /// 保存「插件:渠道」级启用开关。
    pub fn set_channel_enabled(&self, id: &str, channel: &str, value: bool) -> anyhow::Result<()> {
        self.mutate(|plugins| {
            plugins
                .entry(id.to_string())
                .or_insert_with(|| PluginInstance::new(id, now_ms()))
                .channels
                .insert(channel.to_string(), value);
        })
    }

    /// 复制—修改—持久化—发布：文件写入成功后再替换内存注册表。
    fn mutate<F: FnOnce(&mut HashMap<String, PluginInstance>)>(&self, update: F) -> anyhow::Result<()> {
        let mut plugins = self
            .plugins
            .write()
            .map_err(|_| anyhow::anyhow!("插件注册表锁不可用"))?;
        let mut updated = plugins.clone();
        update(&mut updated);
        let data = serde_json::to_vec_pretty(&updated)?;
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(&self.path, data)?;
        *plugins = updated;
        Ok(())
    }
}

/// 合并插件上报的实时状态与桌面端保存的渠道启用开关。
fn merge_channel_states(
    live: Option<&serde_json::Value>,
    plugin: &PluginInstance,
) -> serde_json::Value {
    let mut states = live.cloned().unwrap_or_else(|| serde_json::json!({}));
    if !states.is_object() {
        states = serde_json::json!({});
    }
    if let Some(map) = states.as_object_mut() {
        // 离线插件没有实时状态：为已保存启用选择的渠道补出条目。
        for (channel, enabled) in plugin.channels.iter() {
            let entry = map
                .entry(channel.clone())
                .or_insert_with(|| serde_json::json!({}));
            if let Some(obj) = entry.as_object_mut() {
                obj.insert("desktopEnabled".to_string(), serde_json::json!(*enabled));
            }
        }
        // 实时状态里存在但未保存过启用选择的渠道：默认启用。
        for item in map.values_mut() {
            if let Some(obj) = item.as_object_mut() {
                obj.entry("desktopEnabled".to_string())
                    .or_insert_with(|| serde_json::json!(true));
            }
        }
    }
    states
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn non_empty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::{ClientInfo, ExtensionManager};
    use std::fs;
    use std::path::PathBuf;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("poolman-plugins-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn client(id: &str) -> ClientInfo {
        ClientInfo {
            client_id: Some(id.to_string()),
            name: Some("Poolman 浏览器插件".to_string()),
            version: Some("1.2.3".to_string()),
            platform: Some("chrome".to_string()),
        }
    }

    #[test]
    fn register_persists_connection_info_and_reuses_state() {
        let dir = temp_dir();
        let dir_string = dir.to_string_lossy().to_string();
        let manager = ExtensionManager::new(&dir_string);
        assert_eq!(manager.register(&client("ext-1")), "ext-1");

        manager.update_status(
            "ext-1",
            serde_json::json!({ "deepseek-web": { "connected": true, "authed": true } }),
        );
        assert!(manager.any_connected());
        manager.set_enabled("ext-1", false).unwrap();
        manager.set_channel_enabled("ext-1", "deepseek-web", false).unwrap();
        manager.disconnect("ext-1");
        assert!(!manager.any_connected());

        // 重启后仍保留身份、连接次数与启用选择
        let restored = ExtensionManager::new(&dir_string);
        let listed = restored.list();
        let plugin = listed
            .iter()
            .find(|p| p["id"] == "ext-1")
            .expect("应保留已登记的插件");
        assert_eq!(plugin["enabled"], serde_json::json!(false));
        assert_eq!(plugin["connected"], serde_json::json!(false));
        assert_eq!(plugin["name"], serde_json::json!("Poolman 浏览器插件"));
        assert_eq!(plugin["version"], serde_json::json!("1.2.3"));
        assert_eq!(plugin["connectCount"], serde_json::json!(1));
        assert_eq!(
            plugin["channels"]["deepseek-web"]["desktopEnabled"],
            serde_json::json!(false)
        );

        // 重新连接：复用同一实例，连接次数累加，启用选择保留
        assert_eq!(restored.register(&client("ext-1")), "ext-1");
        let listed = restored.list();
        let plugin = listed.iter().find(|p| p["id"] == "ext-1").unwrap();
        assert_eq!(plugin["connectCount"], serde_json::json!(2));
        assert_eq!(plugin["connected"], serde_json::json!(true));
        assert_eq!(plugin["enabled"], serde_json::json!(false));

        fs::remove_file(dir.join("plugins.json")).unwrap();
        fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn snapshot_merges_status_and_enablement() {
        let dir = temp_dir();
        let dir_string = dir.to_string_lossy().to_string();
        let manager = ExtensionManager::new(&dir_string);
        manager.register(&client("ext-9"));
        manager.set_channel_enabled("ext-9", "chatgpt-web", false).unwrap();
        manager.update_status(
            "ext-9",
            serde_json::json!({ "chatgpt-web": { "connected": true } }),
        );

        let snapshot = manager.snapshot();
        assert!(snapshot.statuses.contains_key("ext-9"));
        assert_eq!(snapshot.enabled.get("ext-9"), Some(&true));
        assert_eq!(snapshot.provider_enabled.get("ext-9:chatgpt-web"), Some(&false));
        assert_eq!(
            manager.latest_status()["chatgpt-web"]["connected"],
            serde_json::json!(true)
        );
        assert_eq!(
            manager.status_of("ext-9").unwrap()["chatgpt-web"]["connected"],
            serde_json::json!(true)
        );

        fs::remove_file(dir.join("plugins.json")).unwrap();
        fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn connections_without_client_id_stay_in_memory() {
        let dir = temp_dir();
        let dir_string = dir.to_string_lossy().to_string();
        let manager = ExtensionManager::new(&dir_string);
        let first = manager.register(&ClientInfo::default());
        let second = manager.register(&ClientInfo::default());
        assert_ne!(first, second);

        let listed = manager.list();
        let plugin = listed.iter().find(|p| p["id"] == first).expect("临时连接应出现在列表中");
        assert_eq!(plugin["connected"], serde_json::json!(true));

        // 未自报身份的连接不落库
        assert!(!dir.join("plugins.json").exists());
        fs::remove_dir(dir).unwrap();
    }
}
