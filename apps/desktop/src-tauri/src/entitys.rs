//! 全部数据表实体（sea-orm + SQLite）：一个数据库 model 一个文件。
//!
//! 集中放全部表结构，便于一眼看清本地库有哪些表与字段；各表的读写归属各自的管理器：
//!
//! | 子模块 | 表 | 归属 |
//! | --- | --- | --- |
//! | [`api_key`] | `api_keys` | `apikey::ApiKeyStore` |
//! | [`session`] | `sessions` | `session::manager::SessionManager` |
//! | [`raw_message`] | `raw_messages` | 同上 |
//! | [`converted_message`] | `converted_messages` | 同上 |
//! | [`provider`] | `providers` | `providers::manager::ProviderManager` |
//! | [`model`] | `models` | 同上 |
//! | [`web_session`] | `web_sessions` | 同上（网页渠道会话池槽位） |
//! | [`request_record`] | `request_records` | `request_record::RequestRecordStore` |
//! | [`local_session`] | `local_sessions` | `local_session::LocalSessionStore` |
//! | [`local_session_content`] | `local_session_contents` | 同上（会话内容） |
//!
//! 每张表一个子模块：`DeriveEntityModel` 会把 `Entity` / `Column` / `ActiveModel` / `Relation`
//! 生成在结构体所在的作用域，多个表放在同一作用域会重名，因此按 sea-orm 的惯例分层。

pub mod api_key;
pub mod converted_message;
pub mod local_session;
pub mod local_session_content;
pub mod model;
pub mod provider;
pub mod raw_message;
pub mod request_record;
pub mod session;
pub mod web_session;
