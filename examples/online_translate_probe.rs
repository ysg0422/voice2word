//! 在线翻译链路的手动验证探针。
//!
//! 走的是**应用实际调用**的公开 API（`TranslateEngine::online` →
//! `/chat/completions`），用来对真实服务端做端到端验证：
//!   - 连接自检（probe）；
//!   - 批量翻译与 `[N] 译文` 协议是否被正确解析、回填；
//!   - 翻译格式选择（原文 / 仅译文 / 双语）的实际导出结果。
//!
//! 用法（不把密钥写死在代码里）：
//!   $env:V2W_PROBE_URL="http://host:port/v1"
//!   $env:V2W_PROBE_KEY="sk-..."
//!   $env:V2W_PROBE_MODEL="model-id"
//!   cargo run --offline --example online_translate_probe -- [目标语言]
//!
//! 目标语言默认 "English"。会依次：连接自检 → 翻译 3 条样例 →
//! 打印三种导出格式。任一步失败都以非零退出码结束，方便手工回归。

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use voice2word::engines::{OnlineApiConfig, TranslateEngine};
use voice2word::subtitle::{ExportMode, Segment};

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

fn main() {
    let target = std::env::args().nth(1).unwrap_or_else(|| "English".to_string());

    let base = env_or("V2W_PROBE_URL", "");
    let key = env_or("V2W_PROBE_KEY", "");
    let model = env_or("V2W_PROBE_MODEL", "");
    if base.is_empty() || key.is_empty() || model.is_empty() {
        eprintln!("请先设置环境变量 V2W_PROBE_URL / V2W_PROBE_KEY / V2W_PROBE_MODEL");
        std::process::exit(2);
    }
    // 容错拼接：允许基址带或不带 /v1，也允许直接给完整端点
    let endpoint = {
        let b = base.trim().trim_end_matches('/');
        if b.ends_with("/chat/completions") {
            b.to_string()
        } else {
            format!("{b}/chat/completions")
        }
    };
    println!("端点: {endpoint}  模型: {model}  目标语言: {target}");

    let cfg = OnlineApiConfig {
        endpoint,
        api_key: key,
        model,
        batch_size: 20,
        timeout_secs: 120,
    };

    // —— 1. 连接自检 ——
    let engine = TranslateEngine::online(cfg.clone());
    match engine.probe_online() {
        Ok(reply) => println!("[连接自检] OK，模型回显: {}", reply.trim()),
        Err(e) => {
            eprintln!("[连接自检] 失败: {e:#}");
            std::process::exit(1);
        }
    }

    // —— 2. 批量翻译（与真实字幕同形）——
    let samples = [
        "你好，欢迎使用音视频智能字幕生成器。",
        "这是一个测试，用来验证翻译链路。",
        "第三句：请保持原意与语气，不要合并或遗漏。",
    ];
    let segs: Vec<Segment> = samples
        .iter()
        .enumerate()
        .map(|(i, t)| Segment::new(i + 1, i as f64, i as f64 + 1.0, *t))
        .collect();

    let t0 = std::time::Instant::now();
    let out = match engine.translate_subtitles(
        segs,
        &target,
        Some(Box::new(|p, msg| println!("   进度 {:.0}% — {msg}", p * 100.0))),
        Arc::new(AtomicBool::new(false)),
    ) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[翻译] 失败: {e:#}");
            std::process::exit(1);
        }
    };
    println!("[翻译] 耗时 {:.1}s", t0.elapsed().as_secs_f64());

    let done = out.iter().filter(|s| s.has_translation()).count();
    println!("[翻译] 译出 {done} / {} 条", out.len());
    for s in &out {
        println!(
            "   [{}] {}\n        → {}",
            s.index,
            s.text,
            s.translation.as_deref().unwrap_or("(未译)")
        );
    }
    if done < out.len() {
        eprintln!("!!! 仍有 {} 条未译出。", out.len() - done);
        std::process::exit(1);
    }

    // —— 3. 导出格式（与界面选择同源）——
    println!("\n—— 导出格式预览 ——");
    for (label, mode) in [
        ("仅原文", ExportMode::RawOnly),
        ("仅译文", ExportMode::TranslationOnly),
        ("双语对照", ExportMode::Bilingual),
    ] {
        println!("[{label}]");
        for s in &out {
            println!("{}", s.export_text(mode));
        }
        println!();
    }
    println!("全部 OK");
}