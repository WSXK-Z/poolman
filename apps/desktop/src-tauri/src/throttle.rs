//! 对话请求频率限制：网页渠道对提问频率有上限，提问太密会直接报错。
//!
//! 每个渠道一条串行通道（[`ChatThrottle`]）：同一时刻只允许一个对话在途，
//! 上一个对话**结束**后再等待配置的间隔才放行下一个，也就是
//! 「对话完成后 x 秒内不再提新问题」。间隔从完成时刻算起，而不是从发起时刻——
//! 对话本身跑了多久都不影响，只保证两轮之间有足够的喘息时间。
//!
//! 频率值由 [`ChatFrequency`] 全渠道共享一份并持久化到数据目录的 `settings.json`——
//! 桌面端「配置 → 对话请求频率」修改后立即生效，重启后沿用；环境变量
//! `CHAT_INTERVAL_MS` 优先级更高，只在进程启动时读取。
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
use tokio::sync::{Mutex, OwnedMutexGuard};
use tokio::time::Instant;

/// 默认的对话完成后等待间隔（毫秒）。
pub const DEFAULT_CHAT_INTERVAL_MS: u64 = 1_000;

/// 间隔上限（毫秒）：配置写错时也不至于把请求排到几小时后。
pub const MAX_CHAT_INTERVAL_MS: u64 = 600_000;

/// 设置文件名（数据目录下）。
const SETTINGS_FILE: &str = "settings.json";

/// 设置项键名。
const CHAT_INTERVAL_KEY: &str = "chat_interval_ms";

/// 对话请求频率设置：全渠道共享一份，修改后对之后排队的请求立即生效。
pub struct ChatFrequency {
    /// 上一个对话完成后需要等待的间隔（毫秒）；0 表示不限制。
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

    /// 为一个渠道建立独立串行通道：频率共用，通道各走各的，互不阻塞。
    pub fn throttle(self: &Arc<Self>) -> ChatThrottle {
        ChatThrottle::new(self.clone())
    }
}

/// 一个渠道的对话串行通道。
///
/// `wait` 获取通道后，先等「上一个对话完成时刻 + 间隔」，再返回 guard；
/// guard 在对话结束时 drop，这一刻才记为新的完成时刻。因此并发请求会自动排队：
/// 后到的必须等前一个真正跑完，而不是按发起顺序排时刻。
pub struct ChatThrottle {
    /// 共用的频率设置。
    frequency: Arc<ChatFrequency>,
    /// 串行门：同一渠道同一时刻只允许一个对话在途。
    gate: Arc<Mutex<()>>,
    /// 上一次对话结束的时刻；None 表示本进程还没完成过对话。
    last_finished: Arc<Mutex<Option<Instant>>>,
}

impl ChatThrottle {
    pub fn new(frequency: Arc<ChatFrequency>) -> Self {
        Self {
            frequency,
            gate: Arc::new(Mutex::new(())),
            last_finished: Arc::new(Mutex::new(None)),
        }
    }

    /// 当前生效的间隔：重试退避等场景据此决定等待时长。
    pub fn interval(&self) -> Duration {
        self.frequency.interval()
    }

    /// 排到通道上，等上一个对话完成满一个间隔后放行。
    ///
    /// 返回的 guard 必须持有到本轮对话结束（`let _guard = ...`，别直接丢弃），
    /// 它 drop 的时刻就是下一次计时的起点。频率为 0（不限制）时返回 `None`，
    /// 不占通道也不等待。
    pub async fn wait(&self) -> Option<ThrottleGuard> {
        let interval = self.frequency.interval();
        if interval.is_zero() {
            return None;
        }
        // 拿不到通道就一直排着：保证「完成时刻」始终只对应一个对话。
        let gate = self.gate.clone().lock_owned().await;
        let scheduled = {
            let last = self.last_finished.lock().await;
            last.map(|finished| finished + interval)
        };
        if let Some(scheduled) = scheduled {
            let now = Instant::now();
            if scheduled > now {
                tokio::time::sleep_until(scheduled).await;
            }
        }
        Some(ThrottleGuard {
            _gate: gate,
            last_finished: self.last_finished.clone(),
        })
    }
}

/// 对话进行中的凭据：drop 即宣告本轮完成，并记下完成时刻。
pub struct ThrottleGuard {
    /// 持有串行门直到对话结束。
    _gate: OwnedMutexGuard<()>,
    /// 完成时刻的存放处。
    last_finished: Arc<Mutex<Option<Instant>>>,
}

impl Drop for ThrottleGuard {
    fn drop(&mut self) {
        // drop 不能 await；try_lock 失败说明有别的锁持有者，此时跳过记录即可，
        // 下一次 wait 会按「没有完成记录」立即放行，最坏是多放一次而非死锁。
        if let Ok(mut last) = self.last_finished.try_lock() {
            *last = Some(Instant::now());
        }
    }
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
        clamp_interval, read_interval, resolve_interval, ChatFrequency, ChatThrottle,
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

    /// 间隔从上一个对话**结束**时刻算起：对话本身跑了 3 秒，下一个也只等配置的 1 秒。
    #[tokio::test(start_paused = true)]
    async fn next_request_waits_interval_after_previous_finishes() {
        let frequency = ChatFrequency::in_memory(1_000);
        let throttle = frequency.throttle();

        let first = throttle.wait().await;
        // 对话本身耗时 3 秒，这期间不应有人被放进通道
        tokio::time::advance(Duration::from_secs(3)).await;
        drop(first);

        let start = Instant::now();
        let second = throttle.wait().await;
        assert_eq!(
            start.elapsed(),
            Duration::from_millis(1_000),
            "间隔应从上一个对话结束时算起，而不是从发起时刻"
        );
        drop(second);
    }

    /// 完成已过去很久时不再补等。
    #[tokio::test(start_paused = true)]
    async fn idle_channel_releases_immediately() {
        let frequency = ChatFrequency::in_memory(1_000);
        let throttle = frequency.throttle();

        let first = throttle.wait().await;
        drop(first);
        tokio::time::advance(Duration::from_secs(5)).await;

        let start = Instant::now();
        let second = throttle.wait().await;
        assert_eq!(start.elapsed(), Duration::ZERO);
        drop(second);
    }

    /// 并发的第二个请求要等前一个真正完成，而不是按发起顺序立即排到。
    #[tokio::test(start_paused = true)]
    async fn concurrent_requests_serialize_on_completion() {
        let frequency = ChatFrequency::in_memory(1_000);
        let throttle = Arc::new(frequency.throttle());

        let first = throttle.wait().await;
        let waiting = tokio::spawn({
            let throttle = throttle.clone();
            async move { throttle.wait().await }
        });
        // 让出执行权：第二个请求此时应卡在串行门上
        tokio::time::advance(Duration::from_millis(10)).await;
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished(), "前一个对话未结束时不能放行");

        // 前一个完成后再等一个间隔
        let start = Instant::now();
        drop(first);
        let second = waiting.await.expect("排队任务不应 panic");
        assert_eq!(start.elapsed(), Duration::from_millis(1_000));
        drop(second);
    }

    /// 0 表示不限制：不等待、不占通道。
    #[tokio::test(start_paused = true)]
    async fn zero_interval_never_waits() {
        let frequency = ChatFrequency::in_memory(1_000);
        let throttle = frequency.throttle();

        let first = throttle.wait().await;
        drop(first);
        frequency.set_interval_ms(0).unwrap();

        let start = Instant::now();
        assert!(throttle.wait().await.is_none());
        assert_eq!(start.elapsed(), Duration::ZERO);
    }

    /// 改频率对之后的请求立即生效，且各渠道各走各的通道。
    #[tokio::test(start_paused = true)]
    async fn new_interval_applies_per_channel() {
        let frequency = ChatFrequency::in_memory(1_000);
        frequency.set_interval_ms(2_500).unwrap();

        let first: ChatThrottle = frequency.throttle();
        let second = frequency.throttle();

        let first_guard = first.wait().await;
        drop(first_guard);

        let start = Instant::now();
        let second_guard = first.wait().await;
        assert_eq!(start.elapsed(), Duration::from_millis(2_500));
        drop(second_guard);

        // 另一条渠道没有完成记录，不受第一条渠道的冷却影响
        let start = Instant::now();
        let other = second.wait().await;
        assert_eq!(start.elapsed(), Duration::ZERO);
        drop(other);
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
