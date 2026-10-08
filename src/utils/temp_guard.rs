//! RAII 临时文件 / 目录守卫。
//!
//! # 为什么需要它
//!
//! 转写链路会在 `%TEMP%` 下写一批中间产物：压实后的 WAV (`v2w_prep_*.wav`)、
//! whisper 的 JSON (`v2w_whisper_*.json`)、切块目录 (`v2w_chunks_*`)、
//! 整轨临时 WAV (`*_voice2word.wav`)、LLM 提示词 (`v2w_prompt_*.txt`) 等。
//!
//! 过去这些文件的删除代码都写在函数的**正常收尾**处，于是任何 `?` 提前返回、
//! 取消分支、`spawn_blocking` 任务被丢弃、或 panic 都会跳过删除，文件永久留在
//! 用户磁盘上（实测一次失败的转写留下 13.8 MB 的压实 WAV）。
//!
//! 修这类问题的正确姿势不是「在每个 `return` / `?` 前补一行 `remove_file`」——
//! 那种改法只要漏掉一条路径就重新漏一个文件，而且后续新增分支时又会忘。
//! 这里改为 RAII：把「文件的生命周期」绑定到一个栈变量上，无论函数从哪条路径
//! 退出（含 panic 展开），栈变量的 `Drop` 都会执行，删除就一定会发生。
//!
//! 需要「把文件交给后续流程」（例如临时 WAV 要传给转写器、转写完才删）时，
//! 把守卫本身 move 给接收方即可——清理责任随所有权自动转移；确实不想再清理时
//! 用 [`TempPathGuard::disarm`] 显式放弃。
//!
//! # 失败处理
//!
//! 删除失败**不会 panic**，只记一条 `warn!`：清理是尽力而为的兜底，
//! 让它把整个转写流程炸掉是本末倒置。文件不存在（`NotFound`）视为正常，
//! 因为调用方可能已经手动删过一次。

use std::path::{Path, PathBuf};
use tracing::warn;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// 单个文件，`remove_file`
    File,
    /// 目录，`remove_dir_all`（递归）
    Dir,
}

/// 离开作用域时自动删除目标文件 / 目录的守卫。
///
/// 见模块文档了解为什么必须用 RAII 而不是在每个返回点手动删。
#[derive(Debug)]
pub struct TempPathGuard {
    path: PathBuf,
    kind: Kind,
    /// `false` 表示所有权已交出 / 已手动清理，`Drop` 不再动作
    armed: bool,
}

impl TempPathGuard {
    /// 守卫一个临时**文件**：`Drop` 时 `remove_file`。
    pub fn file<P: Into<PathBuf>>(path: P) -> Self {
        Self {
            path: path.into(),
            kind: Kind::File,
            armed: true,
        }
    }

    /// 守卫一个临时**目录**：`Drop` 时 `remove_dir_all`（递归删掉里面所有切片）。
    pub fn dir<P: Into<PathBuf>>(path: P) -> Self {
        Self {
            path: path.into(),
            kind: Kind::Dir,
            armed: true,
        }
    }

    /// 查看被守卫的路径（守卫仍持有清理责任）。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 取消自动清理但不需要路径（例如文件已由其它代码删掉）。
    ///
    /// 消费式 API：调用后守卫即失效并被丢弃，语义是「不再由我负责清理」。
    /// 若要把「清理责任」转交给另一个所有者，直接把守卫 move 过去即可，
    /// 因此不需要单独的 `into_path` 之类的解包方法。
    pub fn disarm(mut self) {
        self.armed = false;
    }

    /// 立刻删除并取消后续清理。
    ///
    /// 用于「临时文件在函数中段就不再需要」的场景，比等到函数返回更早释放磁盘。
    pub fn remove_now(&mut self) {
        if self.armed {
            self.delete_quietly();
            self.armed = false;
        }
    }

    fn delete(&self) -> std::io::Result<()> {
        match self.kind {
            Kind::File => std::fs::remove_file(&self.path),
            Kind::Dir => std::fs::remove_dir_all(&self.path),
        }
    }

    /// 删除并吞掉错误（只记日志），供 `Drop` / [`Self::remove_now`] 使用。
    fn delete_quietly(&self) {
        match self.delete() {
            Ok(()) => {}
            // 目标本来就不存在属于正常情况（守卫创建早于文件真正落盘，
            // 或调用方已经删过一次），不算清理失败。
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => warn!(
                path = %self.path.display(),
                error = %e,
                "临时文件清理失败（已忽略，不影响流程）"
            ),
        }
    }
}

impl Drop for TempPathGuard {
    fn drop(&mut self) {
        if self.armed {
            self.delete_quietly();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    /// 每个测试用独立文件名，避免并行执行时互相踩。
    fn scratch(tag: &str) -> PathBuf {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "v2w_guard_test_{}_{}_{}",
            std::process::id(),
            tag,
            n
        ))
    }

    fn make_file(tag: &str) -> PathBuf {
        let p = scratch(tag);
        std::fs::write(&p, b"payload").expect("写入测试文件");
        p
    }

    /// 模拟管线里最常见的场景：函数在中途用 `?` 提前返回。
    fn early_return_after_creating(path: &Path) -> Result<(), &'static str> {
        let _guard = TempPathGuard::file(path);
        // 这个 `?` 就是过去漏删文件的元凶：它跳过了函数末尾的 remove_file
        Err("simulated failure")?;
        #[allow(unreachable_code)]
        Ok(())
    }

    #[test]
    fn deletes_file_when_function_returns_early() {
        let path = make_file("early");
        assert!(path.exists());
        assert!(early_return_after_creating(&path).is_err());
        assert!(!path.exists(), "提前返回时守卫必须删掉临时文件");
    }

    #[test]
    fn keeps_file_while_guard_is_still_alive() {
        // 正常路径：守卫在作用域内时绝不能删文件（否则转写会读到不存在的音频）
        let path = make_file("alive");
        {
            let guard = TempPathGuard::file(&path);
            assert!(guard.path().exists(), "守卫存活期间文件必须存在");
            assert!(path.exists());
        }
        assert!(!path.exists(), "离开作用域后守卫负责清理");
    }

    #[test]
    fn keeps_file_when_disarmed() {
        let path = make_file("disarm");
        {
            let guard = TempPathGuard::file(&path);
            guard.disarm();
        }
        assert!(path.exists(), "disarm 之后守卫不能再删文件");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn remove_now_deletes_immediately() {
        let path = make_file("now");
        let mut guard = TempPathGuard::file(&path);
        guard.remove_now();
        assert!(!path.exists(), "remove_now 必须立刻删除");
        // 之后 drop 不应再报错、也不应 panic
        drop(guard);
    }

    #[test]
    fn dir_guard_removes_directory_recursively() {
        let dir = scratch("dir");
        std::fs::create_dir_all(dir.join("nested")).expect("建测试目录");
        std::fs::write(dir.join("a.wav"), b"a").unwrap();
        std::fs::write(dir.join("nested/b.wav"), b"b").unwrap();
        assert!(dir.exists());

        {
            let _guard = TempPathGuard::dir(&dir);
            // 模拟切片阶段失败：直接 `?` 返回，跳过后面的 remove_dir_all
        }
        assert!(!dir.exists(), "目录守卫必须递归删掉整个切块目录");
    }

    #[test]
    fn dropping_nonexistent_path_does_not_panic() {
        let missing = scratch("missing");
        drop(TempPathGuard::file(&missing));
        drop(TempPathGuard::dir(&missing));
    }

    #[test]
    fn remove_now_on_missing_path_does_not_panic() {
        let missing = scratch("missing_now");
        let mut guard = TempPathGuard::file(&missing);
        guard.remove_now();
        drop(guard);
    }
}
