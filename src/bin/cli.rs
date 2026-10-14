//! 无头 / 脚本专用的命令行入口（**控制台子系统**）。
//!
//! # 为什么不直接用 `voice2word.exe`
//!
//! `voice2word.exe` 是 windows 子系统（GUI）可执行文件：**PowerShell 不会等待它**，
//! 因此 `voice2word doctor --json > out.json` 这类重定向会在子进程写 stdout 之前就被
//! 拆掉管道——文件是空的，还伴随一句 `failed printing to stdout: 管道正在被关闭
//! (os error 232)`。（`cmd /c ... > file` 与 `| Out-File` 恰好能躲过去，所以这个坑
//! 只在 PowerShell 的 `>` 下稳定复现，最容易被当成「程序没输出」。）
//!
//! 控制台子系统的进程由 shell **同步等待**：重定向、管道、`$LASTEXITCODE` 全部正常。
//! 本入口与主程序的 CLI 共用同一套 [`voice2word::cli::run`]，参数、stdout 契约、退出码
//! 完全一致；唯一的差别是这里**没有图形界面**。

fn main() {
    match voice2word::cli::run(std::env::args()) {
        Some(code) => std::process::exit(code),
        // 主程序拿到 `None` 会去起 GUI；这个入口没有 GUI 可起，如实提示用哪个 exe，
        // 而不是静默什么都不做（那看起来就是「双击/运行没反应」）。
        None => {
            // 借 `--help` 打印同一份用法文本：手抄一份必然与实现漂移。
            let _ = voice2word::cli::run(["voice2word-cli".to_string(), "--help".to_string()]);
            eprintln!("（这是无头入口：要打开图形界面请运行 voice2word.exe）");
            std::process::exit(voice2word::cli::EXIT_USAGE);
        }
    }
}
