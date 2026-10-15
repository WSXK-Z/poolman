//! 插件调度：把「哪台插件该处理这次请求」从 WebSocket 收发里剥出来。
//!
//! 选择逻辑是**纯函数**，便于单测；收发放仍然由 [`crate::extension::websocket::WebSocketBridge`] 负责。
//! 完整策略见 `链路设计.md` 4.5 节：过滤 → 排序（会话粘性 → 最少在途 → 最近成功）。
//!
//! 尚未接入的部分（后续步骤）：在途计数、并发上限与等待队列、失败重试、会话粘性落库。

/// 一台插件作为候选时的快照。
///
/// 由桥接层从 [`crate::extension::manager::ExtensionManager`] 的注册表快照
/// 以及调度层自己的运行时指标（`inflight` / `sticky` / `last_ok_at`）组装而来。
#[derive(Debug, Clone)]
pub struct Candidate {
    pub plugin_id: String,
    /// 插件级启用开关
    pub enabled: bool,
    /// 「插件:供应商」级启用开关
    pub provider_enabled: bool,
    /// 插件自报：该供应商页面已连接
    pub connected: bool,
    /// 插件自报：该供应商已登录
    pub authed: bool,
    /// 插件自报可用的模型（空表示不限制）
    pub models: Vec<String>,
    /// 在途请求数（桌面端计数）
    pub inflight: u32,
    /// 该会话上次成功使用的插件（会话粘性）
    pub sticky: bool,
    /// 最近一次成功时间戳（毫秒），越大越近
    pub last_ok_at: i64,
}

/// 为一次请求选定插件。
///
/// 1. **过滤**：插件启用 ∧ 供应商启用 ∧ 已连接 ∧ 已登录 ∧（模型能力匹配，未上报则不限制）
/// 2. **排序**：会话粘性 → 在途最少 → 最近成功
pub fn pick_plugin<'a>(candidates: &'a [Candidate], model: &str) -> Option<&'a Candidate> {
    candidates
        .iter()
        .filter(|c| c.enabled && c.provider_enabled && c.connected && c.authed)
        .filter(|c| {
            // 调用方未提供模型名（空串）时不做模型能力过滤；
            // 插件未上报 models 时同样不限制。
            model.is_empty() || c.models.is_empty() || c.models.iter().any(|m| m == model)
        })
        .min_by(|a, b| {
            (!a.sticky)
                .cmp(&(!b.sticky))
                .then(a.inflight.cmp(&b.inflight))
                .then(b.last_ok_at.cmp(&a.last_ok_at))
        })
}

#[cfg(test)]
mod tests {
    use super::{pick_plugin, Candidate};

    fn candidate(id: &str) -> Candidate {
        Candidate {
            plugin_id: id.to_string(),
            enabled: true,
            provider_enabled: true,
            connected: true,
            authed: true,
            models: Vec::new(),
            inflight: 0,
            sticky: false,
            last_ok_at: 0,
        }
    }

    #[test]
    fn filters_out_unavailable_plugins() {
        let mut unauthed = candidate("unauthed");
        unauthed.authed = false;
        let mut unsupported_provider = candidate("provider-off");
        unsupported_provider.provider_enabled = false;
        let mut disconnected = candidate("disconnected");
        disconnected.connected = false;
        let usable = candidate("usable");

        // 候选先落到具名 Vec：pick_plugin 返回的引用借用它，不能直接借用临时数组
        let candidates = vec![unauthed, unsupported_provider, disconnected, usable];
        let picked = pick_plugin(&candidates, "gpt-4o").expect("应有一台可用插件");
        assert_eq!(picked.plugin_id, "usable");
    }

    #[test]
    fn prefers_sticky_then_least_inflight_then_most_recent() {
        // 会话粘性优先，即使在途更多
        let mut sticky = candidate("sticky");
        sticky.sticky = true;
        sticky.inflight = 5;
        let fresh = candidate("fresh");
        assert_eq!(
            pick_plugin(&[fresh.clone(), sticky], "gpt-4o")
                .unwrap()
                .plugin_id,
            "sticky"
        );

        // 无粘性时选在途最少的
        let mut busy = candidate("busy");
        busy.inflight = 3;
        let mut idle = candidate("idle");
        idle.inflight = 1;
        assert_eq!(
            pick_plugin(&[busy, idle], "gpt-4o").unwrap().plugin_id,
            "idle"
        );

        // 在途相同则选最近成功的
        let mut older = candidate("older");
        older.last_ok_at = 100;
        let mut recent = candidate("recent");
        recent.last_ok_at = 200;
        assert_eq!(
            pick_plugin(&[older, recent], "gpt-4o").unwrap().plugin_id,
            "recent"
        );
    }

    #[test]
    fn respects_reported_model_capability() {
        let mut limited = candidate("limited");
        limited.models = vec!["claude-3-opus".to_string()];
        let open = candidate("open");

        // limited 不支持 gpt-4o → 选 open
        assert_eq!(
            pick_plugin(&[limited.clone(), open], "gpt-4o")
                .unwrap()
                .plugin_id,
            "open"
        );
        // 只有 limited 支持该模型时仍会被选中
        assert_eq!(
            pick_plugin(&[limited], "claude-3-opus").unwrap().plugin_id,
            "limited"
        );
        // 未上报模型列表的插件不限制
        assert!(pick_plugin(&[candidate("any")], "unknown-model").is_some());
    }

    /// 调用方不知道模型名（空串，如 EXECUTE_REQUEST 不带对外模型名时）不应被插件上报的模型列表挡掉。
    #[test]
    fn empty_model_skips_capability_filter() {
        let mut limited = candidate("limited");
        limited.models = vec!["claude-3-opus".to_string()];
        assert_eq!(pick_plugin(&[limited], "").unwrap().plugin_id, "limited");
    }

    #[test]
    fn returns_none_when_nothing_usable() {
        let mut disabled = candidate("disabled");
        disabled.enabled = false;
        assert!(pick_plugin(&[disabled], "gpt-4o").is_none());
        assert!(pick_plugin(&[], "gpt-4o").is_none());
    }
}
