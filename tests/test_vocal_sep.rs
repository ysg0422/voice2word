//! 人声分离 runner 的集成测试：`--self-test` 必须真实执行并通过。
//!
//! runner 是仓库内脚本（`tools/vocal_sep_runner.py`），必须存在——缺了就是
//! checkout / 打包出了问题，不能像外部资产那样静默 SKIP；只有 Python 解释器
//! （外部资产）缺失时才走 `tests/common` 的显式 SKIP 口径。

use std::process::Command;
use voice2word::engines::VocalSepEngine;
use voice2word::utils::AppConfig;

mod common;

#[test]
fn vocal_sep_runner_self_test_passes() {
    let runner = AppConfig::resolve_path("tools/vocal_sep_runner.py");
    let python = AppConfig::resolve_command("python");

    assert!(
        runner.is_file(),
        "tools/vocal_sep_runner.py 是仓库内脚本，必须存在：{}",
        runner.display()
    );
    // 优雅跳过：只可能是解释器缺失（外部环境问题）。
    if common::skip_if_missing(
        "vocal_sep_runner_self_test_passes",
        &[("python 解释器", &python)],
    ) {
        return;
    }

    let out = Command::new(&python)
        .arg(&runner)
        .arg("--self-test")
        .output()
        .expect("运行 vocal_sep_runner --self-test 失败");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "self-test 必须通过（DSP 往返 / 重叠相加常数和 / 恒等假模型全流程）\nstdout: {stdout}\nstderr: {stderr}"
    );

    // 契约复核：stdout 末行必须是 `{"status":"ok",...}` 的结果 JSON。
    let last = stdout
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("");
    let value: serde_json::Value = serde_json::from_str(last.trim())
        .unwrap_or_else(|e| panic!("stdout 末行不是 JSON: {e}（{last}）"));
    assert_eq!(
        value.get("status").and_then(|v| v.as_str()),
        Some("ok"),
        "stdout 末行 JSON 的 status 必须是 ok：{last}"
    );
}

/// 引擎可用性口径：runner 是仓库内脚本，is_available 只被模型文件左右。
#[test]
fn vocal_sep_engine_availability_matches_model_presence() {
    let missing = AppConfig::resolve_path("models/vocal_sep/definitely-missing.onnx");
    let engine = VocalSepEngine::new("cpu".to_string(), missing);
    assert!(!engine.is_available(), "模型缺失时必须判不可用");

    let probe =
        std::env::temp_dir().join(format!("vocal_sep_it_probe_{}.onnx", std::process::id()));
    std::fs::write(&probe, b"probe").expect("写占位模型文件");
    let engine = VocalSepEngine::new("cpu".to_string(), probe.clone());
    assert!(engine.is_available(), "runner 与模型都在 ⇒ 必须可用");
    let _ = std::fs::remove_file(&probe);
}
