//! 浏览器扩展通信、凭证管理、供应商控制、插件调度及网页平台适配。
pub mod entitys;
// 旧适配器树（巨石 `deepseek.rs` + 拆分中的 `deepseek/`）已不在运行期链路上：
// 网页渠道由 `models/` 直接实现，请求经 `executor.rs` 下发，无任何模块引用本树。
// 它正处在 `converter → converters / prompts` 的迁移中间态、无法编译，故暂不参与构建；
// 搬迁完成后删除本树（见「架构重设计.md」阶段 3）。
// pub mod adapters;
pub mod credentials;
pub mod executor;
pub mod handler;
pub mod provider_control;
pub mod scheduler;
pub mod websocket;
pub mod manager;
