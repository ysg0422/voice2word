//! 辅助子进程登记处：收容那些「发起后没人 `wait()`」的短命进程。
//!
//! # 为什么需要它
//!
//! `src/ui/actions.rs` 的字幕预览会 `spawn()` 一个 `ffplay` 进程。`Child` 句柄
//! 一旦被丢弃，进程就成了孤儿：`-autoexit` 只在「正常播完」时生效，用户在看预览的
//! 时候关掉应用、或反复点预览，就会在系统里留下一串没人回收的 `ffplay.exe`
//! （占着视频文件句柄与音频设备）。孤儿进程还会在父进程退出后继续存活，
//! 因此**必须由我们显式 `kill()` 并 `wait()` 回收**。
//!
//! 这里不把句柄塞进 `AppState`，而是放在模块级全局里，原因有二：
//! 1. 句柄的生命周期是「进程级」的，不属于任何一次渲染状态；
//! 2. 应用退出收尾在 `main.rs`（`Application::run` 返回之后），那里拿不到 `AppState`。
//!
//! 登记进来的进程都必须 `kill()` + `wait()` 成对执行：只 kill 不 wait 会留下
//! 未回收的进程条目（句柄未关闭，`tasklist` 里仍可见）。

use std::process::Child;
use std::sync::Mutex;
use tracing::warn;

/// 全局登记表。`Mutex<Vec<Child>>` 的构造是 const，因此可以直接做 static。
static CHILDREN: Mutex<Vec<Child>> = Mutex::new(Vec::new());

/// 登记一个由本应用发起、但不会有人 `wait()` 的辅助进程。
///
/// 登记时会顺手回收已经自行退出的条目，避免反复预览把表撑大。
pub fn register(child: Child) {
    let mut list = CHILDREN.lock().unwrap_or_else(|e| e.into_inner());
    reap_locked(&mut list);
    list.push(child);
}

/// 终止并回收所有登记进程。
///
/// 两处调用：启动新的预览播放器之前（避免反复预览堆积出一串 ffplay），
/// 以及应用退出时（`main.rs` 在 `Application::run` 返回后调用）。
pub fn retire_all() {
    let mut list = CHILDREN.lock().unwrap_or_else(|e| e.into_inner());
    for mut child in list.drain(..) {
        if let Err(e) = child.kill() {
            // 进程可能已经自己退出，kill 失败不是错误
            warn!(error = %e, "终止辅助子进程失败（可能已自行退出）");
        }
        // kill 之后必须 wait，否则句柄不释放
        if let Err(e) = child.wait() {
            warn!(error = %e, "回收辅助子进程失败");
        }
    }
}

/// 当前登记的进程数（仅测试使用，用于断言回收确实发生）。
#[cfg(test)]
pub fn count() -> usize {
    let list = CHILDREN.lock().unwrap_or_else(|e| e.into_inner());
    list.len()
}

/// 去掉 `try_wait()` 已返回退出码的条目。`try_wait` 同时完成 wait 回收。
fn reap_locked(list: &mut Vec<Child>) {
    list.retain_mut(|child| match child.try_wait() {
        Ok(Some(_)) => false, // 已退出，句柄已回收
        Ok(None) => true,     // 仍在运行
        Err(_) => false,      // 句柄异常，丢弃避免泄漏
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};
    use std::time::Duration;

    /// 全局登记表是进程级单例，两个测试并行跑会互相看到对方的进程，
    /// 因此用一把测试锁把它们串行化。
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    // 这些测试只验证登记/回收机制本身，只在 Windows 上跑（项目目标平台）。
    #[cfg(target_os = "windows")]
    fn spawn_short_lived() -> Child {
        // `cmd /C exit` 会立刻退出，用来验证 reap 路径
        Command::new("cmd")
            .args(["/C", "exit", "0"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn cmd")
    }

    #[cfg(target_os = "windows")]
    fn spawn_long_lived() -> Child {
        // ping 会阻塞约 30 秒，足够验证 retire_all 真的杀了它
        Command::new("cmd")
            .args(["/C", "ping", "-n", "30", "127.0.0.1"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn ping")
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn reaps_finished_children_on_next_register() {
        let _serial = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        register(spawn_short_lived());
        assert_eq!(count(), 1);
        // 等它自行退出，再登记一个新进程：register 内部应先回收掉旧条目
        std::thread::sleep(Duration::from_millis(800));
        register(spawn_short_lived());
        assert_eq!(count(), 1, "已退出的子进程必须在下次登记时被回收，不能越堆越多");
        retire_all();
        assert_eq!(count(), 0);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn retire_all_kills_running_children() {
        let _serial = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        register(spawn_long_lived());
        assert_eq!(count(), 1);
        let started = std::time::Instant::now();
        retire_all();
        assert_eq!(count(), 0);
        // 若 retire_all 只是「移除条目」而没有 kill，这个长跑进程会拖到超时；
        // 这里断言它被立即终止了。
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "retire_all 必须在 kill+wait 后立即返回"
        );
    }
}
