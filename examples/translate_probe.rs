//! 离线 Qwen 翻译链路的手工验证探针。
//!
//! 它走的是**应用实际调用**的公开 API（`LLMEngine::translate` → 常驻 llama-server），
//! 而不是单测里那些纯函数，用来复现/回归以下问题：
//!   - 输出 token 上限与批次大小脱钩，长批译文被**静默截断**（少几句）。
//!
//! 用法：
//!   cargo run --release --example translate_probe -- [每行字符数] [行数]
//! 默认「150 字 × 24 行」——这是修复前必然触发截断的压力批。

use voice2word::engines::LLMEngine;
use voice2word::subtitle::Segment;
use voice2word::utils::AppConfig;

fn main() {
    let mut args = std::env::args().skip(1);
    let per_line: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(150);
    let rows: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(24);

    let cfg = AppConfig::load_from_file("config.toml").unwrap_or_default();
    let cli = AppConfig::resolve_path(&cfg.paths.llama_cli);
    let model = AppConfig::resolve_path(&cfg.paths.llm_model);
    if !cli.exists() || !model.exists() {
        eprintln!("llama-cli 或模型不存在：{cli:?} / {model:?}");
        std::process::exit(2);
    }

    // 造一段稳定的中文长句（与真实课程字幕的字符密度接近）
    let unit = "这是一段用于压力测试的较长中文字幕内容，用来模拟真实课程里那种一口气说很久、\
                单句字数明显偏多的语音片段，包含标点与常见口语表达。";
    let body: String = unit.chars().cycle().take(per_line).collect();
    let segs: Vec<Segment> = (1..=rows)
        .map(|i| Segment::new(i, 0.0, 1.0, &body))
        .collect();
    let src_chars: usize = segs.iter().map(|s| s.text.chars().count()).sum();
    println!("输入：{rows} 行 × {per_line} 字 = {src_chars} 字符");

    let engine = LLMEngine::new(
        cli,
        model,
        cfg.pipeline.llm_ctx,
        cfg.pipeline.llm_threads,
    );

    let t0 = std::time::Instant::now();
    let out = engine
        .translate(segs, "English", None)
        .expect("翻译应有返回");
    let elapsed = t0.elapsed().as_secs_f64();

    let done = out
        .iter()
        .filter(|s| s.translation.as_deref().map(|t| !t.trim().is_empty()).unwrap_or(false))
        .count();
    println!("译出 {done} / {rows} 行，耗时 {elapsed:.1}s");
    if done < rows {
        eprintln!("!!! 仍有 {} 行未译出（修复目标是 0）", rows - done);
        std::process::exit(1);
    }
    println!("全部译出，OK");
}
