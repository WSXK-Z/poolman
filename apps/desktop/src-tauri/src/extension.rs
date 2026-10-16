//! 浏览器插件连接的维护与管理。
//!
//! 本模块只做一件事：把浏览器插件的连接管好。它不实现任何网页平台逻辑——
//! 网页渠道由 [`crate::models`] 直接实现，请求经 [`executor`] 下发到插件执行。
//!
//! | 子模块 | 职责 |
//! | --- | --- |
//! | [`manager`] | 插件实例注册表：身份与连接信息、实时状态、插件级/「插件:渠道」级启停，持久化到 `plugins.json` |
//! | [`websocket`] | 插件连接本身：WebSocket 服务端、下行请求与上行回执的收发、在途计数与会话粘性 |
//! | [`handler`] | 上行消息分发：执行结果 / 流式增量 / 凭证同步 / 状态同步 |
//! | [`credentials`] | 插件捕获的凭证（进程内，按渠道隔离） |
//! | [`executor`] | 渠道执行器：把模型侧的渠道请求经连接下发到已登录网页 |
//! | [`scheduler`] | 选插件：过滤 + 排序的纯函数（会话粘性 → 最少在途 → 最近成功） |
//!
//! 渠道启停（哪个渠道参与请求分配）属于供应商状态，见 [`crate::providers::provider_control`]。

pub mod credentials;
pub mod executor;
pub mod handler;
pub mod manager;
pub mod scheduler;
pub mod websocket;
