use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

#[tokio::test]
async fn test_full_pipeline_run() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let config_path = manifest_dir.join("config.toml");
    let config_str = std::fs::read_to_string(&config_path).expect("读取 config.toml 失败");
    let config: toml::Value = toml::from_str(&config_str).expect("解析 config.toml 失败");

    let ffmpeg_path = config["paths"]["ffmpeg"].as_str().unwrap();
    let whisper_cli = config["paths"]["whisper_cli"].as_str().unwrap();
    let whisper_model = manifest_dir.join(config["paths"]["whisper_model"].as_str().unwrap());
    let llama_cli = config["paths"]["llama_cli"].as_str().unwrap();
    let llm_model = manifest_dir.join(config["paths"]["llm_model"].as_str().unwrap());

    println!("FFmpeg: {}", ffmpeg_path);
    println!("Whisper CLI: {}", whisper_cli);
    println!("Whisper Model: {:?}", whisper_model);
    println!("LLM CLI: {}", llama_cli);
    println!("LLM Model: {:?}", llm_model);

    assert!(std::path::Path::new(ffmpeg_path).exists(), "FFmpeg 路径不存在");
    assert!(std::path::Path::new(whisper_cli).exists(), "Whisper CLI 不存在");
    assert!(whisper_model.exists(), "Whisper 模型不存在");
    assert!(std::path::Path::new(llama_cli).exists(), "LLM CLI 不存在");
    assert!(llm_model.exists(), "LLM 模型不存在");

    let sample_mp4 = manifest_dir.join("resources").join("sample").join("sample.mp4");
    assert!(sample_mp4.exists(), "sample.mp4 不存在");

    let out_srt = manifest_dir.join("resources").join("sample").join("sample.srt");

    // 1. FFmpeg 抽取
    println!("\n[1/4] FFmpeg 提取音频...");
    let ffmpeg = voice2word::engines::FFmpegEngine::new(ffmpeg_path);
    let wav = ffmpeg.extract_audio(&sample_mp4, None).expect("提取音频失败");
    println!("音频提取完成: {:?}", wav);

    // 2. Whisper 转写
    println!("\n[2/4] Whisper 识别语音...");
    let whisper = voice2word::engines::WhisperEngine::new(whisper_cli, &whisper_model, 4, 0);
    let (segments, vad_sec) = whisper.transcribe(&wav, Some("zh"), None, None, None).expect("转写失败");
    println!("转写完成，片段数: {}，VAD 耗时: {:.2}s", segments.len(), vad_sec);
    for seg in &segments {
        println!("  #{}: [{:.2} -> {:.2}] {}", seg.index, seg.start, seg.end, seg.text);
    }
    let _ = std::fs::remove_file(&wav);

    assert!(!segments.is_empty(), "未识别出任何语音片段");

    // 3. LLM 润色
    println!("\n[3/4] Qwen LLM 润色...");
    let llm = voice2word::engines::LLMEngine::new(llama_cli, &llm_model, 4096, 4);
    let polished = llm.polish(segments, None).expect("润色失败");
    for seg in &polished {
        println!("  #{}: 原文='{}' -> 润色='{}'", seg.index, seg.text, seg.polished);
    }

    // 4. 导出 SRT
    println!("\n[4/4] 导出标准 SRT 文件...");
    voice2word::subtitle::SubtitleWriter::write_to_file(&polished, &out_srt, "srt").expect("导出 SRT 失败");
    assert!(out_srt.exists(), "SRT 未能成功写入");

    let srt_content = std::fs::read_to_string(&out_srt).expect("读取生成的 SRT 失败");
    println!("\n--- 生成的 SRT 内容 ---:\n{}", srt_content);
    println!("-----------------------");
}
