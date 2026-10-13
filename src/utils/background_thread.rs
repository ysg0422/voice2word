//! 长任务专用线程派发（长活不跟界面抢调度权重）。
//!
//! # 为什么长任务不能走 `cx.background_executor()`
//!
//! gpui 0.2.2 在 Windows 上的 dispatcher 把**每一个**后台任务都按
//! `WorkItemPriority::High` 派发（见 `platform/windows/dispatcher.rs`）：短任务
//! （几百微秒～几毫秒）这样没问题，但「跑几十秒以上」的活会和转写中的 whisper
//! 抢调度权重——whisper 已经吃满核，这些任务再加柴，界面就发涩。
//!
//! 因此长任务统一走本模块：`std::thread::Builder` 起**专用命名线程**，并在
//! Windows 上把该线程降到 `THREAD_PRIORITY_BELOW_NORMAL`（与
//! `engines/media_pipeline.rs::apply_background_priority` 对子进程的做法同源，
//! 只是对象从子进程换成当前线程）。任务本身的逻辑与结果不变；结束回 UI 写状态
//! 仍由调用方在 `this.update(cx, ...)` 里完成。

use tokio::sync::oneshot;

/// 必须走专用线程（而不是 GPUI `background_executor`）的长任务清单：
/// 线程名 -> 为什么这个任务算「长任务」。
///
/// 这份清单既是**可断言的策略**（单测校验调用点用的线程名都登记在案），也是
/// 给未来的自己留的口径：拿不准某任务该不该离开后台执行器时，先对照这里。
pub(crate) const DEDICATED_LONG_TASKS: &[(&str, &str)] = &[
    (
        "v2w-waveform-extract",
        "时间轴波形要把整段音轨解码成 PCM 再分桶聚合，长视频是几十秒级 CPU 活",
    ),
    (
        "v2w-preview-proxy",
        "预览代理是整片 FFmpeg 转码，耗时随视频时长线性增长，动辄几分钟",
    ),
    (
        "v2w-model-presence-scan",
        "重扫模型就位状态会对声明了 sha256 的条目做全文件哈希，几百 MB 顺序读",
    ),
    (
        "v2w-frame-prefetch",
        "时间轴预取会真起一次 FFmpeg 抽下一格，属于「提前做」的活，不能占提交路径",
    ),
];

/// 线程名是否登记在 [`DEDICATED_LONG_TASKS`] 里。
pub(crate) fn is_dedicated_long_task(thread_name: &str) -> bool {
    DEDICATED_LONG_TASKS
        .iter()
        .any(|(name, _)| *name == thread_name)
}

/// 起一条专用线程执行长任务，返回可 await 的结果接收端。
///
/// - 任务正常结束时结果经 oneshot 回传；线程起不来或任务 panic 时接收端以
///   `Err` 结束，调用方按「任务失败」处理即可，不会挂死。
/// - 线程名必须登记在 [`DEDICATED_LONG_TASKS`]（debug 构建下断言）。
/// - 只在任务体内降优先级，不影响调用线程（通常是 UI 线程）。
pub(crate) fn spawn_long_task<F, R>(thread_name: &str, job: F) -> oneshot::Receiver<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    let (tx, rx) = oneshot::channel();
    spawn_detached_long_task(thread_name, move || {
        let _ = tx.send(job());
    });
    rx
}

/// 起一条专用线程跑长任务，不要结果（fire-and-forget）。
pub(crate) fn spawn_detached_long_task<F>(thread_name: &str, job: F)
where
    F: FnOnce() + Send + 'static,
{
    debug_assert!(
        is_dedicated_long_task(thread_name),
        "长任务线程 {thread_name} 未登记在 DEDICATED_LONG_TASKS"
    );
    let spawned = std::thread::Builder::new()
        .name(thread_name.to_string())
        .spawn(move || {
            lower_current_thread_priority();
            job();
        });
    if let Err(err) = spawned {
        // 线程都起不来属于资源耗尽级故障：记日志，接收端（若有）会立刻 Err，
        // 调用方按任务失败回报，不会静默卡住。
        tracing::error!(thread = thread_name, error = %err, "长任务专用线程启动失败");
    }
}

/// Windows：把**当前线程**降到 BELOW_NORMAL，给前台与转写让路。
///
/// 非 Windows 上是空操作（主目标平台是 Windows，其它平台保持原行为）。
fn lower_current_thread_priority() {
    #[cfg(windows)]
    {
        use windows::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
        };
        // SAFETY: `GetCurrentThread` 返回当前线程的伪句柄（无需关闭），
        // `SetThreadPriority` 只改本线程调度权重，无内存安全问题。
        unsafe {
            let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{is_dedicated_long_task, spawn_long_task, DEDICATED_LONG_TASKS};
    use std::collections::HashSet;

    /// 清单必须覆盖所有「该走专用线程」的任务，且每条都写清理由。
    #[test]
    fn long_task_registry_covers_every_dedicated_dispatcher() {
        let names: Vec<&str> = DEDICATED_LONG_TASKS.iter().map(|(n, _)| *n).collect();
        let unique: HashSet<&str> = names.iter().copied().collect();
        assert_eq!(unique.len(), names.len(), "线程名不允许重复");
        for expected in [
            "v2w-waveform-extract",
            "v2w-preview-proxy",
            "v2w-model-presence-scan",
            "v2w-frame-prefetch",
        ] {
            assert!(
                is_dedicated_long_task(expected),
                "{expected} 必须登记为长任务"
            );
        }
        assert!(
            DEDICATED_LONG_TASKS
                .iter()
                .all(|(_, why)| !why.trim().is_empty()),
            "每条长任务都必须写清为什么算长任务"
        );
        // 反例：常驻的短任务（资源监控）不属于这份清单。
        assert!(!is_dedicated_long_task("v2w-resource-monitor"));
    }

    /// 结果要回传，任务必须跑在**指定的命名线程**上。
    #[test]
    fn spawn_long_task_runs_on_named_thread_and_returns_result() {
        let rx = spawn_long_task("v2w-waveform-extract", || {
            let name = std::thread::current().name().map(str::to_string);
            (name, 40 + 2)
        });
        let (name, value) = rx.blocking_recv().expect("长任务结果应回传");
        assert_eq!(name.as_deref(), Some("v2w-waveform-extract"));
        assert_eq!(value, 42);
    }

    /// Windows 上专用线程必须真的降到 BELOW_NORMAL（对界面/转写让路）。
    #[cfg(windows)]
    #[test]
    fn spawn_long_task_lowers_thread_priority_on_windows() {
        use windows::Win32::System::Threading::{
            GetCurrentThread, GetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
        };
        let rx = spawn_long_task("v2w-preview-proxy", || {
            // SAFETY: 只读当前线程优先级，无内存/句柄安全问题。
            unsafe { GetThreadPriority(GetCurrentThread()) }
        });
        assert_eq!(
            rx.blocking_recv().expect("长任务结果应回传"),
            THREAD_PRIORITY_BELOW_NORMAL.0,
            "专用线程必须降到 BELOW_NORMAL"
        );
    }
}
