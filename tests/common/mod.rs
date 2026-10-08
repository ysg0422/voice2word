//! 集成测试共享辅助：外部资产缺失 / CI 环境下的**显式跳过**。
//!
//! # 模块规则
//! `tests/common/mod.rs` 不是独立的测试 target（放在子目录里，只有被
//! `mod common;` 引用时才编译）。每个 `tests/*.rs` 都写 `mod common;`，于是这个
//! 文件会被**分别编译进每一个集成测试 crate**；某个文件用不到的辅助函数在那一份
//! 里就是 `dead_code`。这里在文件顶部用 `#![allow(dead_code)]` 一次性压掉，好处是
//! 新增辅助函数不会再冒出一堆 warning，也不必去动 workspace 级别的 lint 配置。
//!
//! # 观测性约定（实测结论：SKIP 必须**直接写真实 stderr 句柄**）
//! 本机实测（rustc/cargo 1.98 + libtest），对一个**通过**的测试：
//!
//! | 写法 | 默认 `cargo test` | `--nocapture` | 测试失败时 |
//! |---|---|---|---|
//! | `println!`（宏 → stdout） | ✗ 不可见 | ✓ | ✓ 回放 |
//! | `eprintln!`（宏 → stderr） | ✗ 不可见 | ✓ | ✓ 回放 |
//! | `writeln!(std::io::stderr(), ..)` | ✓ 可见 | ✓ | ✓ |
//!
//! 也就是说 libtest 的 `set_output_capture` 会同时接管 stdout **和** stderr
//! （两个宏都走 `std::io::_print` / `_eprint`，会先查线程本地的捕获槽）；
//! 只有绕过宏、**直接向 `std::io::stderr()` 句柄写入**才会落到真实 fd 2、
//! 不被捕获。因此本模块的 [`report_skip`] 用真实 stderr 句柄写入，
//! 保证 `cargo test`（含 CI 日志）里**一定**看得见 SKIP——这是「绿了但没跑」
//! 唯一可观测的信号，不能只靠 `--nocapture`。
//!
//! # 跳过策略（两个条件，任一命中即跳过）
//! 1. `VOICE2WORD_CI` 置位（见 [`is_ci`]）：CI checkout 之后 `models/`、
//!    `resources/`、`testVideo/` 都是空目录，依赖真实资产的集成测试无从执行；
//! 2. 逐个资产 `Path::exists()` 兜底：本机也可能没下载模型。
//!
//! 两个条件都**不改变**「资产在就必须真跑」的语义：条件不命中时，测试主体原样
//! 执行，所有断言保持。

// 每个集成测试 crate 各编译一份，用不到的辅助函数必然 dead_code，统一压掉。
#![allow(dead_code)]

use std::io::Write;
use std::path::Path;

/// 读取一个布尔环境变量：存在且不为 `0` / `false` / 空串时即为真。
pub fn env_flag(name: &str) -> bool {
    match std::env::var(name) {
        Ok(v) => {
            let v = v.trim();
            !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false")
        }
        Err(_) => false,
    }
}

/// 是否处于 CI（由 `.github/workflows/ci.yml` 设置 `VOICE2WORD_CI=1`）。
pub fn is_ci() -> bool {
    env_flag("VOICE2WORD_CI")
}

/// 打印一条 SKIP 信息。
///
/// **必须**直接写 `std::io::stderr()` 句柄（而不是 `eprintln!`）：libtest 的
/// 输出捕获对 `println!`/`eprintln!` 都生效，只有绕过宏写真实句柄才能保证
/// 默认 `cargo test`（通过态）下可见。详见模块级文档。
pub fn report_skip(test_name: &str, detail: &str) {
    let mut err = std::io::stderr();
    let _ = writeln!(err, "SKIP: {detail}，跳过 {test_name}");
    let _ = err.flush();
}

/// 若处于 CI 环境：打印 SKIP 并返回 `true`（调用方应尽早 `return`）。
pub fn skip_in_ci(test_name: &str) -> bool {
    if is_ci() {
        report_skip(test_name, "VOICE2WORD_CI 已置位（CI 无外部资产）");
        return true;
    }
    false
}

/// 资产清单里任一缺失：打印 `SKIP: 缺少 <资产>（<路径>），跳过 <测试名>` 并返回 `true`。
pub fn skip_if_missing(test_name: &str, assets: &[(&str, &Path)]) -> bool {
    if let Some(&(name, path)) = assets.iter().find(|(_, p)| !p.exists()) {
        report_skip(test_name, &format!("缺少 {name}（{}）", path.display()));
        return true;
    }
    false
}

/// 统一的「CI **或** 资产缺失 → 跳过」判据。命中任一条即返回 `true`。
///
/// 用法：把本测试需要的资产列成 `&[(&str, &Path)]`，命中就 `return`；
/// 不命中时后续代码原样执行（因此「资产在 = 真跑」的语义被保留）。
pub fn skip_heavy(test_name: &str, assets: &[(&str, &Path)]) -> bool {
    skip_in_ci(test_name) || skip_if_missing(test_name, assets)
}

/// 长耗时基准的总开关：默认**不跑**，只有 `V2W_RUN_LONG_BENCH=1` 才真跑。
/// 返回 `true` 表示「已跳过」。用于 32 分钟长视频这类本机也不该默认跑的用例。
pub fn skip_long_bench(test_name: &str, detail: &str) -> bool {
    if env_flag("V2W_RUN_LONG_BENCH") {
        return false;
    }
    report_skip(
        test_name,
        &format!("{detail}（长基准默认关闭，设 V2W_RUN_LONG_BENCH=1 才执行）"),
    );
    true
}
