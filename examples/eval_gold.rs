//! 对 5:00–15:00 的 Whisper 基准输出与人工校对字幕计算中文 CER。
//! cargo run --offline --example eval_gold
//! cargo run --offline --example eval_gold -- testVideo/03.1.3概率不等式.srt

use anyhow::{Context, Result};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

const CLIP_START: f64 = 300.0;
const EVAL_START: f64 = 305.0;
const EVAL_END: f64 = 895.0;

fn parse_time(value: &str) -> Option<f64> {
    let value = value.trim().replace(',', ".");
    let parts: Vec<_> = value.split(':').collect();
    if parts.len() != 3 {
        return None;
    }
    Some(
        parts[0].parse::<f64>().ok()? * 3600.0
            + parts[1].parse::<f64>().ok()? * 60.0
            + parts[2].parse::<f64>().ok()?,
    )
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

fn read_whisper_json(path: &Path) -> Result<Vec<(f64, f64, String)>> {
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
            CLIP_START + start / 1000.0,
            CLIP_START + end / 1000.0,
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
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let gold_path = root.join("testVideo/03.1.3概率不等式_success.srt");
    let bench_dir = root.join("target/whisper_speed_bench");
    let gold = read_srt(&gold_path)?;
    if let Some(candidate) = std::env::args().nth(1) {
        let candidate_path = root.join(candidate);
        let hypothesis = read_srt(&candidate_path)?;
        let (reference, gold_segments) = normalized_between(&gold, 0.0, f64::INFINITY);
        let (prediction, predicted_segments) = normalized_between(&hypothesis, 0.0, f64::INFINITY);
        let edits = edit_distance(&reference, &prediction);
        println!("标准: {} ({}句)", gold_path.display(), gold_segments);
        println!(
            "待测: {} ({}句)",
            candidate_path.display(),
            predicted_segments
        );
        println!(
            "全片 CER: {:.2}% ({} / {} 字)",
            edits as f64 / reference.chars().count() as f64 * 100.0,
            edits,
            reference.chars().count()
        );
        return Ok(());
    }
    let (reference, gold_segments) = normalized_between(&gold, EVAL_START, EVAL_END);
    let reference_len = reference.chars().count();
    println!(
        "标准字幕: {} | 评估区间 05:05–14:55 | 句数 {} | 归一化字数 {}",
        gold_path.display(),
        gold_segments,
        reference_len
    );
    println!("倍率  VAD   句数   字数   编辑距离   CER");
    for (rate, vad) in [
        ("1_00", "0_50"),
        ("1_15", "0_50"),
        ("1_25", "0_50"),
        ("1_35", "0_50"),
        ("1_35", "0_55"),
        ("1_35", "0_60"),
        ("1_50", "0_50"),
    ] {
        let path = bench_dir.join(format!("result_x{rate}_vad{vad}.json"));
        if !path.exists() {
            continue;
        }
        let (hypothesis, segments) =
            normalized_between(&read_whisper_json(&path)?, EVAL_START, EVAL_END);
        let distance = edit_distance(&reference, &hypothesis);
        println!(
            "{:<5} {:<5} {:>4} {:>6} {:>10} {:>6.2}%",
            rate.replace('_', "."),
            vad.replace('_', "."),
            segments,
            hypothesis.chars().count(),
            distance,
            distance as f64 / reference_len as f64 * 100.0,
        );
    }
    for (label, file) in [
        ("1.00*", "result_x1_00_vad0_50_math.json"),
        ("1.25*", "result_x1_25_vad0_50_math.json"),
        ("Q5-1", "result_x1_00_vad0_50_q5_p2.json"),
        ("Q5-1.25", "result_x1_25_vad0_50_q5_p2.json"),
        ("Small-Q5", "result_x1_00_vad0_50_q5_small.json"),
        ("Small-Q4", "result_x1_00_vad0_50_q4_small.json"),
        ("Base-Q5", "result_x1_00_vad0_50_base_q5.json"),
        ("Small-Q5-mc0", "result_x1_00_vadmc0_q5_small.json"),
        ("Small-Q5-vt55", "result_x1_00_vadvt055_q5_small.json"),
        ("preVAD", "pre_vad_0.50_mapped.json"),
        ("preVAD+", "pre_vad_0.55_mapped.json"),
    ] {
        let path = bench_dir.join(file);
        if !path.exists() {
            continue;
        }
        let (hypothesis, segments) =
            normalized_between(&read_whisper_json(&path)?, EVAL_START, EVAL_END);
        let distance = edit_distance(&reference, &hypothesis);
        println!(
            "{label:<5} {:<5} {:>4} {:>6} {:>10} {:>6.2}%",
            "0.50",
            segments,
            hypothesis.chars().count(),
            distance,
            distance as f64 / reference_len as f64 * 100.0
        );
    }
    Ok(())
}
