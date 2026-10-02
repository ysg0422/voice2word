//! Whisper 并行配置的质量对照：对同一段音频的不同 `-t/-p` 输出计算中文 CER。
//!
//! 与 `eval_gold.rs` 共用同一套归一化（繁转简 → 只保留字母数字 → 小写），
//! 但评估窗口可配置，便于对任意切片做 A/B 对照。
//!
//! 用法：
//!   cargo run --offline --example eval_whisper_parallel -- <json目录> [片段起点秒] [评估起点秒] [评估终点秒]
//!
//! 例（对 05:00 起 6 分钟切片、评估 05:05–10:55）：
//!   cargo run --offline --example eval_whisper_parallel -- /tmp/bench 300 305 655

use anyhow::{Context, Result};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

fn parse_time(value: &str) -> Option<f64> {
    let value = value.trim().replace(',', ".");
    let parts: Vec<_> = value.split(':').collect();
    if parts.len() != 3 {
        return None;
    }
    Some(parts[0].parse::<f64>().ok()? * 3600.0
        + parts[1].parse::<f64>().ok()? * 60.0
        + parts[2].parse::<f64>().ok()?)
}

fn read_srt(path: &Path) -> Result<Vec<(f64, f64, String)>> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("读取标准字幕失败: {}", path.display()))?;
    let mut result = Vec::new();
    for block in raw
        .trim_start_matches('\u{feff}')
        .replace("\r\n", "\n")
        .split("\n\n")
    {
        let mut lines = block.lines();
        let Some(time_line) = lines.find(|line| line.contains("-->")) else {
            continue;
        };
        let Some((start, end)) = time_line.split_once("-->") else {
            continue;
        };
        let (Some(start), Some(end)) = (parse_time(start), parse_time(end)) else {
            continue;
        };
        let text = lines.collect::<Vec<_>>().join("");
        result.push((start, end, text));
    }
    Ok(result)
}

fn read_whisper_json(path: &Path, clip_start: f64) -> Result<Vec<(f64, f64, String)>> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("读取 Whisper JSON 失败: {}", path.display()))?;
    let value: Value = serde_json::from_str(&raw)?;
    let mut result = Vec::new();
    for item in value["transcription"]
        .as_array()
        .context("JSON 缺少 transcription")?
    {
        let Some(start) = item["offsets"]["from"].as_f64() else {
            continue;
        };
        let Some(end) = item["offsets"]["to"].as_f64() else {
            continue;
        };
        let Some(text) = item["text"].as_str() else {
            continue;
        };
        result.push((
            clip_start + start / 1000.0,
            clip_start + end / 1000.0,
            text.to_string(),
        ));
    }
    Ok(result)
}

fn normalized_between(items: &[(f64, f64, String)], from: f64, to: f64) -> (String, usize) {
    let mut text = String::new();
    let mut count = 0;
    for (start, end, part) in items {
        let midpoint = (start + end) / 2.0;
        if !(from..to).contains(&midpoint) {
            continue;
        }
        text.push_str(part);
        count += 1;
    }
    let simplified = zhconv::zhconv(&text, zhconv::Variant::ZhHans);
    let normalized = simplified
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect();
    (normalized, count)
}

fn edit_distance(reference: &str, prediction: &str) -> usize {
    let a: Vec<char> = reference.chars().collect();
    let b: Vec<char> = prediction.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut row = vec![0; b.len() + 1];
    for (i, &left) in a.iter().enumerate() {
        row[0] = i + 1;
        for (j, &right) in b.iter().enumerate() {
            row[j + 1] = (prev[j + 1] + 1)
                .min(row[j] + 1)
                .min(prev[j] + usize::from(left != right));
        }
        std::mem::swap(&mut prev, &mut row);
    }
    prev[b.len()]
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dir = PathBuf::from(args.first().context(
        "用法: eval_whisper_parallel <json目录> [片段起点秒] [评估起点秒] [评估终点秒]",
    )?);
    let clip_start: f64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(300.0);
    let eval_start: f64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(305.0);
    let eval_end: f64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(655.0);

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let gold_path = root.join("testVideo/03.1.3概率不等式_success.srt");
    let gold = read_srt(&gold_path)?;
    let (reference, gold_segments) = normalized_between(&gold, eval_start, eval_end);
    let reference_len = reference.chars().count();
    println!(
        "标准字幕: {} | 评估区间 {:.0}s–{:.0}s | 句数 {} | 归一化字数 {}",
        gold_path.display(),
        eval_start,
        eval_end,
        gold_segments,
        reference_len
    );
    println!(
        "{:<14} {:>6} {:>8} {:>10} {:>9}",
        "配置", "句数", "字数", "编辑距离", "CER"
    );

    let mut entries: Vec<PathBuf> = fs::read_dir(&dir)
        .with_context(|| format!("读取目录失败: {}", dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map(|x| x == "json").unwrap_or(false))
        .collect();
    entries.sort();

    for path in entries {
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_string();
        let items = match read_whisper_json(&path, clip_start) {
            Ok(items) => items,
            Err(err) => {
                println!("{name:<14} 读取失败: {err}");
                continue;
            }
        };
        let (hypothesis, segments) = normalized_between(&items, eval_start, eval_end);
        let distance = edit_distance(&reference, &hypothesis);
        println!(
            "{:<14} {:>6} {:>8} {:>10} {:>8.2}%",
            name,
            segments,
            hypothesis.chars().count(),
            distance,
            distance as f64 / reference_len as f64 * 100.0
        );
    }
    Ok(())
}
