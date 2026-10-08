use std::path::PathBuf;

mod common;

#[tokio::test]
async fn test_full_pipeline_run() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // 必须走应用**真实的**配置加载路径：`AppConfig::load_from_file` 会把
    // `config.local.toml`（本机绝对路径，不进版本库）逐项叠加到 `config.toml` 之上。
    // 直接读 `config.toml` 的原始 TOML 会漏掉这层覆盖——凡是把 ffmpeg / llama.cpp
    // 配在 `config.local.toml` 的机器，这个「全链路」测试都会在第一步就假失败。
    std::env::set_current_dir(&manifest_dir).expect("切换工作目录到项目根失败");
    let cfg = voice2word::utils::AppConfig::load_from_file("config.toml")
        .expect("加载配置失败（config.toml + config.local.toml）");

    let ffmpeg_path = voice2word::utils::AppConfig::resolve_path(&cfg.paths.ffmpeg);
    let whisper_cli = voice2word::utils::AppConfig::resolve_path(&cfg.paths.whisper_cli);
    let whisper_model = voice2word::utils::AppConfig::resolve_path(&cfg.paths.whisper_model);
    let llama_cli = voice2word::utils::AppConfig::resolve_path(&cfg.paths.llama_cli);
    let llm_model = voice2word::utils::AppConfig::resolve_path(&cfg.paths.llm_model);

    println!("FFmpeg: {:?}", ffmpeg_path);
    println!("Whisper CLI: {:?}", whisper_cli);
    println!("Whisper Model: {:?}", whisper_model);
    println!("LLM CLI: {:?}", llama_cli);
    println!("LLM Model: {:?}", llm_model);

    let sample_mp4 = manifest_dir
        .join("resources")
        .join("sample")
        .join("sample.mp4");

    // 优雅跳过：CI（`VOICE2WORD_CI=1`，checkout 后无资产）或本机缺任一外部资产时，
    // 打印 SKIP（直接写真实 stderr 句柄，默认 `cargo test` 下即可见）并尽早 return，
    // 而不是让「全链路」测试因为环境缺件而假失败。
    if common::skip_heavy(
        "test_full_pipeline_run",
        &[
            ("FFmpeg", &ffmpeg_path),
            ("Whisper CLI", &whisper_cli),
            ("Whisper 模型", &whisper_model),
            ("LLM CLI", &llama_cli),
            ("LLM 模型", &llm_model),
            ("sample.mp4", &sample_mp4),
        ],
    ) {
        return;
    }

    // 资产齐备：以下断言与全链路执行全部保留（资产在 → 必须真跑，绝不当空壳）。
    assert!(ffmpeg_path.exists(), "FFmpeg 路径不存在: {ffmpeg_path:?}");
    assert!(whisper_cli.exists(), "Whisper CLI 不存在: {whisper_cli:?}");
    assert!(
        whisper_model.exists(),
        "Whisper 模型不存在: {whisper_model:?}"
    );
    assert!(llama_cli.exists(), "LLM CLI 不存在: {llama_cli:?}");
    assert!(llm_model.exists(), "LLM 模型不存在: {llm_model:?}");
    assert!(sample_mp4.exists(), "sample.mp4 不存在");

    let out_srt = manifest_dir
        .join("resources")
        .join("sample")
        .join("sample.srt");

    // 1. FFmpeg 抽取
    println!("\n[1/4] FFmpeg 提取音频...");
    let ffmpeg = voice2word::engines::FFmpegEngine::new(ffmpeg_path.to_string_lossy().as_ref());
    let wav = ffmpeg
        .extract_audio(&sample_mp4, None)
        .expect("提取音频失败");
    println!("音频提取完成: {:?}", wav);

    // 2. Whisper 转写
    println!("\n[2/4] Whisper 识别语音...");
    let whisper = voice2word::engines::WhisperEngine::new(
        whisper_cli.to_string_lossy().as_ref(),
        &whisper_model,
        4,
        0,
    );
    let (segments, vad_sec) = whisper
        .transcribe(&wav, Some("zh"), None, None, None)
        .expect("转写失败");
    println!(
        "转写完成，片段数: {}，VAD 耗时: {:.2}s",
        segments.len(),
        vad_sec
    );
    for seg in &segments {
        println!(
            "  #{}: [{:.2} -> {:.2}] {}",
            seg.index, seg.start, seg.end, seg.text
        );
    }
    let _ = std::fs::remove_file(&wav);

    assert!(!segments.is_empty(), "未识别出任何语音片段");

    // 3. LLM 润色
    println!("\n[3/4] Qwen LLM 润色...");
    let llm = voice2word::engines::LLMEngine::new(
        llama_cli.to_string_lossy().as_ref(),
        &llm_model,
        4096,
        4,
    );
    let polished = llm.polish(segments, None).expect("润色失败");
    for seg in &polished {
        println!(
            "  #{}: 原文='{}' -> 润色='{}'",
            seg.index, seg.text, seg.polished
        );
    }

    // 4. 导出 SRT
    println!("\n[4/4] 导出标准 SRT 文件...");
    voice2word::subtitle::SubtitleWriter::write_to_file(&polished, &out_srt, "srt")
        .expect("导出 SRT 失败");
    assert!(out_srt.exists(), "SRT 未能成功写入");

    let srt_content = std::fs::read_to_string(&out_srt).expect("读取生成的 SRT 失败");
    println!("\n--- 生成的 SRT 内容 ---:\n{}", srt_content);
    println!("-----------------------");
}
