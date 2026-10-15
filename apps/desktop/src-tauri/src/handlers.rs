//! Tauri 命令面的组合层：让命令分组在各文件声明、最后汇总成一次注册。
//!
//! `tauri::generate_handler!` 产出的是「按值消费 `Invoke`、先按命令名自匹配」的闭包，
//! 两次调用的结果无法拼接；`Builder::invoke_handler` 又是覆盖语义（处理器字段私有，
//! 没有读回已注册处理器的入口）。所以同一个应用只能在**一处**登记全部命令。
//!
//! 这里用「先按命令名挑出归属组，再把 `Invoke` 交给该组」绕开拼接限制：
//! 每个命令文件用 [`command_set!`] 建立自己的 [`CommandSet`]，最后由 [`chain_commands!`]
//! 汇总成一个处理器交给 `Builder::invoke_handler`。
//!
//! 分派是安全的：`generate_handler!` 对未声明的命令直接返回 `false`，不会有副作用，
//! 因此同名命令只会被第一组消费，未被任何组声明的命令原样交回 Tauri。

use tauri::ipc::{Invoke, InvokeHandler};
use tauri::Runtime;

/// 一组命令：声明它们时的源码路径（用来得出命令名）与该组的处理器。
pub struct CommandSet<R: Runtime> {
    paths: &'static [&'static str],
    handler: Box<InvokeHandler<R>>,
}

impl<R: Runtime> CommandSet<R> {
    /// `paths` 是 `stringify!` 后的命令路径，命令名取路径的最后一段。
    pub fn new<F>(paths: &'static [&'static str], handler: F) -> Self
    where
        F: Fn(Invoke<R>) -> bool + Send + Sync + 'static,
    {
        Self {
            paths,
            handler: Box::new(handler),
        }
    }

    /// 本组是否声明了 `name` 这条命令。
    fn claims(&self, name: &str) -> bool {
        self.paths
            .iter()
            .any(|path| command_name_matches(path, name))
    }
}

/// `path`（如 `crate::commands::api_keys::create_api_key`）的最后一段是否为 `name`。
fn command_name_matches(path: &str, name: &str) -> bool {
    match path.strip_suffix(name) {
        Some(prefix) => prefix.is_empty() || prefix.ends_with("::"),
        None => false,
    }
}

/// 汇总多组命令：按命令名投递给声明它的那一组。
///
/// 同名命令以先声明的组为准；没有组声明时返回 `false`，交回 Tauri 继续处理。
pub fn chain<R: Runtime>(
    sets: Vec<CommandSet<R>>,
) -> impl Fn(Invoke<R>) -> bool + Send + Sync + 'static {
    move |invoke| {
        let matched = sets
            .iter()
            .position(|set| set.claims(invoke.message.command()));
        match matched {
            Some(index) => (sets[index].handler)(invoke),
            None => false,
        }
    }
}

/// 建立一组命令：既生成该组的处理器，也记下命令名（取路径最后一段）。
///
/// 命令若用 `#[tauri::command(rename = ...)]` 改名，命令名不再等于函数名，此时改用
/// [`CommandSet::new`] 手工登记名称。
macro_rules! command_set {
    ($($command:path),* $(,)?) => {
        $crate::handlers::CommandSet::new(
            &[$(stringify!($command)),*],
            ::tauri::generate_handler![$($command),*],
        )
    };
}

/// 把多个 [`command_set!`] 的结果汇总成一个 Tauri 处理器。
macro_rules! chain_commands {
    ($($set:expr),* $(,)?) => {
        $crate::handlers::chain(vec![$($set),*])
    };
}

pub(crate) use chain_commands;
pub(crate) use command_set;

#[cfg(test)]
mod tests {
    use super::command_name_matches;

    /// 命令名只认路径最后一段，且必须落在段边界上。
    #[test]
    fn matches_only_last_segment() {
        assert!(command_name_matches("greet", "greet"));
        assert!(command_name_matches("create_api_key", "create_api_key"));
        assert!(command_name_matches(
            "crate::commands::api_keys::create_api_key",
            "create_api_key"
        ));
        // 只是后缀但不是同一段
        assert!(!command_name_matches("recreate_api_key", "create_api_key"));
        assert!(!command_name_matches("my_greet", "greet"));
        // 前缀拼出来的名字不算
        assert!(!command_name_matches("other::create_api_keyx", "create_api_key"));
        assert!(!command_name_matches("commands::key", "greet"));
    }
}
