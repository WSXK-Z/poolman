//! 对话请求频率限制：网页渠道对提问频率有上限，提交太快会直接报错。
//!
//! 每个渠道一条排队（[`ChatThrottle`]）：相邻两次对话请求的**发起**间隔不小于配置值，
//! 也就是「每 x 秒最多向同一网页渠道提一个问题」。频率值由 [`ChatFrequency`] 全渠道
//! 共享一份并持久化到数据目录的 `settings.json`——桌面端「配置 → 对话请求频率」修改后
//! 立即生效，重启后沿用；环境变量 `CHAT_INTERVAL_MS` 优先级更高，只在进程启动时读取。
//!
//! 只限制**对话请求**：建会话、拉历史、取 PoW 挑战等辅助请求不排队，否则会把它们
//! 也一起拖慢，而网页端真正限流的是提问。

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::Mutex;
use tokio::time::Instant;

/// 默认的相邻对话请求间隔（毫秒）：网页端每秒最多接受一次提问。
pub const DEFAULT_CHAT_INTERVAL_MS: u64 = 1_000;

/// 间隔上限（毫秒）：配置写错时也不至于把请求排到几小时后。
pub const MAX_CHAT_INTERVAL_MS: u64 = 600_000;

/// 设置文件名（数据目录下）。
const SETTINGS_FILE: &str = "settings.json";

/// 设置项键名。
const CHAT_INTERVAL_KEY: &str = "chat_interval_ms";

/// 对话请求频率设置：全渠道共享一份，修改后对之后排队的请求立即生效。
pub struct ChatFrequency {
    /// 相邻两次对话请求的最小间隔（毫秒）；0 表示不限制。
    interval_ms: AtomicU64,
    /// 持久化文件；None 表示只存在内存里（测试）。
    path: Option<PathBuf>,
}

impl ChatFrequency {
    /// 从数据目录加载：`CHAT_INTERVAL_MS` 环境变量优先于 `settings.json`，两者都没有时用默认值。
    pub fn load(data_dir: &str) -> Arc<Self> {
        let path = Path::new(data_dir).join(SETTINGS_FILE);
        let from_env = env::var("CHAT_INTERVAL_MS")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok());
        let from_file = read_interval(&path);
        Arc::new(Self::at(Some(path), resolve_interval(from_env, from_file)))
    }

    /// 只存在内存里的实例：测试用，也可用于不落盘的场景。
    pub fn in_memory(interval_ms: u64) -> Arc<Self> {
        Arc::new(Self::at(None, interval_ms))
    }

    fn at(path: Option<PathBuf>, interval_ms: u64) -> Self {
        Self {
            interval_ms: AtomicU64::new(clamp_interval(interval_ms)),
            path,
        }
    }

    /// 当前间隔（毫秒）；0 表示不限制。
    pub fn interval_ms(&self) -> u64 {
        self.interval_ms.load(Ordering::Relaxed)
    }

    /// 当前间隔。
    pub fn interval(&self) -> Duration {
        Duration::from_millis(self.interval_ms())
    }

    /// 修改间隔并持久化，返回实际生效的毫秒值（越界值会被夹到 `0..=MAX_CHAT_INTERVAL_MS`）。
    ///
    /// 先落盘再改运行期值：保存失败时行为不变，界面报错即代表这次修改没有生效。
    pub fn set_interval_ms(&self, interval_ms: u64) -> Result<u64, String> {
        let interval_ms = clamp_interval(interval_ms);
        if let Some(path) = &self.path {
            write_interval(path, interval_ms).map_err(|error| {
                format!("保存对话请求频率失败（{}）：{}", path.display(), error)
            })?;
        }
        self.interval_ms.store(interval_ms, Ordering::Relaxed);
        Ok(interval_ms)
    }

    /// 为一个渠道建立独立排队：频率共用，队列各排各的，互不阻塞。
    pub fn throttle(self: &Arc<Self>) -> ChatThrottle {
        ChatThrottle::new(self.clone())
    }
}

/// 一个渠道的对话请求排队：按到达顺序放行，相邻放行间隔为一个频率值。
pub struct ChatThrottle {
    /// 共用的频率设置。
    frequency: Arc<ChatFrequency>,
    /// 队尾的放行时刻；None 表示队列为空。
    tail: Mutex<Option<Instant>>,
}

impl ChatThrottle {
    pub fn new(frequency: Arc<ChatFrequency>) -> Self {
        Self {
            frequency,
            tail: Mutex::new(None),
        }
    }

    /// 当前生效的间隔：重试退避等场景据此决定等待时长。
    pub fn interval(&self) -> Duration {
        self.frequency.interval()
    }

    /// 排到队尾，等到轮到自己再返回。
    ///
    /// 队列空时立即放行，否则接在上一个请求之后一个间隔；取号在锁内完成，
    /// 因此放行顺序与排队顺序一致。频率为 0（不限制）时不等也不占位。
    pub async fn wait(&self) {
        let interval = self.frequency.interval();
        if interval.is_zero() {
            return;
        }
        let scheduled = {
            let mut tail = self.tail.lock().await;
            let (scheduled, next) = reserve(*tail, Instant::now(), interval);
            *tail = Some(next);
            scheduled
        };
        tokio::time::sleep_until(scheduled).await;
    }
}

/// 取号：定出这次请求的放行时刻，并给出队尾的新时刻。
///
/// 队尾已经过去（队列空了一会儿）就地放行，否则接在队尾之后一个间隔。
fn reserve(tail: Option<Instant>, now: Instant, interval: Duration) -> (Instant, Instant) {
    let scheduled = match tail {
        Some(previous) if previous > now => previous,
        _ => now,
    };
    (scheduled, scheduled + interval)
}

/// 读取已保存的间隔；文件不存在、损坏或没有该键时返回 None。
fn read_interval(path: &Path) -> Option<u64> {
    let data = fs::read(path).ok()?;
    let value: Value = serde_json::from_slice(&data).ok()?;
    value.get(CHAT_INTERVAL_KEY).and_then(Value::as_u64)
}

/// 写入设置文件：只改本项，其余键原样保留（文件坏掉时重建）。
fn write_interval(path: &Path, interval_ms: u64) -> std::io::Result<()> {
    let mut settings = fs::read(path)
        .ok()
        .and_then(|data| serde_json::from_slice::<Value>(&data).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    settings.insert(CHAT_INTERVAL_KEY.to_string(), Value::from(interval_ms));
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let text = serde_json::to_string_pretty(&Value::Object(settings))
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    fs::write(path, text)
}

/// 频率来源优先级：环境变量 > 设置文件 > 默认值；结果夹到允许范围。
fn resolve_interval(from_env: Option<u64>, from_file: Option<u64>) -> u64 {
    clamp_interval(from_env.or(from_file).unwrap_or(DEFAULT_CHAT_INTERVAL_MS))
}

/// 夹到允许范围：0（不限制）到 [`MAX_CHAT_INTERVAL_MS`]。
fn clamp_interval(interval_ms: u64) -> u64 {
    interval_ms.min(MAX_CHAT_INTERVAL_MS)
}

#[cfg(test)]
mod tests {
    use super::{
        clamp_interval, read_interval, reserve, resolve_interval, ChatFrequency, ChatThrottle,
        DEFAULT_CHAT_INTERVAL_MS, MAX_CHAT_INTERVAL_MS,
    };
    use serde_json::Value;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::time::Instant;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("poolman-throttle-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 取号：空队列立即放行；接在队尾之后；队尾已过去时按当前时刻排。
    #[test]
    fn reserve_schedules_after_queue_tail() {
        let now = Instant::now();
        let interval = Duration::from_millis(1_000);

        let (scheduled, tail) = reserve(None, now, interval);
        assert_eq!(scheduled, now, "空队列立即放行");
        assert_eq!(tail, now + interval, "队尾顺延一个间隔");

        let (scheduled, tail) = reserve(Some(tail), now, interval);
        assert_eq!(scheduled, now + interval, "接在上一个请求之后");
        assert_eq!(tail, now + interval * 2);

        let (scheduled, tail) = reserve(Some(now - interval), now, interval);
        assert_eq!(scheduled, now, "队尾已过去时按当前时刻排");
        assert_eq!(tail, now + interval);
    }

    /// 连续提问按间隔放行：第一个立即，后面每个都等一个间隔。
    #[tokio::test(start_paused = true)]
    async fn waits_one_interval_between_requests() {
        let frequency = ChatFrequency::in_memory(1_000);
        let throttle = frequency.throttle();

        throttle.wait().await;
        let start = Instant::now();
        throttle.wait().await;
        assert_eq!(start.elapsed(), Duration::from_millis(1_000));

        let start = Instant::now();
        throttle.wait().await;
        assert_eq!(start.elapsed(), Duration::from_millis(1_000));
    }

    /// 队列空了一会儿之后不再补等：间隔从这次请求重新算起。
    #[tokio::test(start_paused = true)]
    async fn idle_queue_releases_immediately() {
        let frequency = ChatFrequency::in_memory(1_000);
        let throttle = frequency.throttle();

        throttle.wait().await;
        tokio::time::advance(Duration::from_secs(5)).await;
        let start = Instant::now();
        throttle.wait().await;
        assert_eq!(start.elapsed(), Duration::ZERO);
    }

    /// 0 表示不限制：不等待，也不占用队尾。
    #[tokio::test(start_paused = true)]
    async fn zero_interval_never_waits() {
        let frequency = ChatFrequency::in_memory(1_000);
        let throttle = frequency.throttle();

        throttle.wait().await;
        frequency.set_interval_ms(0).unwrap();
        let start = Instant::now();
        throttle.wait().await;
        throttle.wait().await;
        assert_eq!(start.elapsed(), Duration::ZERO);
    }

    /// 改频率对之后的请求立即生效，且各渠道各排各的队。
    #[tokio::test(start_paused = true)]
    async fn new_interval_applies_per_channel() {
        let frequency = ChatFrequency::in_memory(1_000);
        frequency.set_interval_ms(2_500).unwrap();

        let first: ChatThrottle = frequency.throttle();
        let second = frequency.throttle();
        first.wait().await;
        // 另一条渠道不受第一条渠道排队影响
        second.wait().await;

        let start = Instant::now();
        first.wait().await;
        assert_eq!(start.elapsed(), Duration::from_millis(2_500));
    }

    /// 设置持久化到 settings.json：重启后沿用，且不丢掉文件里的其他键。
    #[test]
    fn saved_interval_survives_reload() {
        let dir = temp_dir();
        let path = dir.join("settings.json");
        fs::write(&path, r#"{"note":"keep-me"}"#).unwrap();

        let frequency = Arc::new(ChatFrequency::at(
            Some(path.clone()),
            DEFAULT_CHAT_INTERVAL_MS,
        ));
        assert_eq!(frequency.interval_ms(), DEFAULT_CHAT_INTERVAL_MS);
        assert_eq!(frequency.set_interval_ms(4_500).unwrap(), 4_500);

        let saved: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved["chat_interval_ms"], serde_json::json!(4_500));
        assert_eq!(saved["note"], serde_json::json!("keep-me"));
        assert_eq!(read_interval(&path), Some(4_500));

        fs::remove_dir_all(dir).ok();
    }

    /// 设置文件损坏时重建，不影响保存。
    #[test]
    fn broken_settings_file_is_rebuilt() {
        let dir = temp_dir();
        let path = dir.join("settings.json");
        fs::write(&path, "not json").unwrap();

        let frequency = Arc::new(ChatFrequency::at(
            Some(path.clone()),
            DEFAULT_CHAT_INTERVAL_MS,
        ));
        frequency.set_interval_ms(1_500).unwrap();
        assert_eq!(read_interval(&path), Some(1_500));

        fs::remove_dir_all(dir).ok();
    }

    /// 越界值夹到允许范围：0 是不限制，超上限按上限。
    #[test]
    fn interval_is_clamped() {
        assert_eq!(clamp_interval(0), 0);
        assert_eq!(
            clamp_interval(DEFAULT_CHAT_INTERVAL_MS),
            DEFAULT_CHAT_INTERVAL_MS
        );
        assert_eq!(clamp_interval(u64::MAX), MAX_CHAT_INTERVAL_MS);
    }

    /// 优先级：环境变量 > 设置文件 > 默认值。
    #[test]
    fn env_overrides_file_and_default() {
        assert_eq!(resolve_interval(Some(3_000), Some(2_000)), 3_000);
        assert_eq!(resolve_interval(None, Some(2_000)), 2_000);
        assert_eq!(resolve_interval(None, None), DEFAULT_CHAT_INTERVAL_MS);
    }
}
