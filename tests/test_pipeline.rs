use std::path::{Path, PathBuf};

use voice2word::app::WhisperModelTier;
use voice2word::engines::{LLMEngine, SenseVoiceEngine, WhisperEngine};
use voice2word::utils::AppConfig;

mod common;

#[tokio::test]
async fn test_full_pipeline_run() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // 必须走应用**真实的**配置加载路径：`AppConfig::load_from_file` 会把
    // `config.local.toml`（本机绝对路径，不进版本库）逐项叠加到 `config.toml` 之上。
    // 直接读 `config.toml` 的原始 TOML 会漏掉这层覆盖——凡是把 ffmpeg / llama.cpp
    // 配在 `config.local.toml` 的机器，这个「全链路」测试都会在第一步就假失败。
    std::env::set_current_dir(&manifest_dir).expect("切换工作目录到项目根失败");
    let cfg = AppConfig::load_from_file("config.toml")
        .expect("加载配置失败（config.toml + config.local.toml）");

    let ffmpeg_path = AppConfig::resolve_path(&cfg.paths.ffmpeg);
    let whisper_cli = AppConfig::resolve_path(&cfg.paths.whisper_cli);
    let whisper_model = AppConfig::resolve_path(&cfg.paths.whisper_model);
    let llama_cli = AppConfig::resolve_path(&cfg.paths.llama_cli);
    let llm_model = AppConfig::resolve_path(&cfg.paths.llm_model);

    // 默认识别引擎由 `paths.whisper_model` 反推，与应用启动路径
    // （`WhisperModelTier::from_model_path`）**同源**：出厂默认是 SenseVoice，
    // 其模型是 `.onnx`、必须交给 SenseVoice 引擎（Python runner）识别。此前这个
    // 测试无条件把 `paths.whisper_model` 喂给 whisper-cli——默认档一旦是 SenseVoice，
    // 就会拿一个 ONNX 文件当 ggml 模型加载，得到一个与代码无关的「failed to load model」。
    let is_sv = WhisperModelTier::from_model_path(&cfg.paths.whisper_model)
        == Some(WhisperModelTier::SenseVoice);

    let sv_runner = AppConfig::resolve_path("tools/sensevoice_runner.py");
    let sv_model = cfg
        .paths
        .sensevoice_model
        .as_deref()
        .map(AppConfig::resolve_path)
        .unwrap_or_default();
    let sv_tokens = cfg
        .paths
        .sensevoice_tokens
        .as_deref()
        .map(AppConfig::resolve_path)
        .unwrap_or_default();
    let sv_vad = cfg
        .paths
        .sensevoice_vad
        .as_deref()
        .map(AppConfig::resolve_path)
        .unwrap_or_default();

    let sample_mp4 = manifest_dir
        .join("resources")
        .join("sample")
        .join("sample.mp4");

    // 优雅跳过：CI（`VOICE2WORD_CI=1`，checkout 后无资产）或本机缺任一外部资产时，
    // 打印 SKIP（直接写真实 stderr 句柄，默认 `cargo test` 下即可见）并尽早 return，
    // 而不是让「全链路」测试因为环境缺件而假失败。
    //
    // 资产清单按**当前默认引擎**取：SenseVoice 需要 runner + 模型/词表/VAD；
    // whisper.cpp 需要 whisper-cli + ggml 模型。两者不混列，否则任一档位下都会
    // 因为列了「另一条路径的资产」而误判缺件。
    let mut assets: Vec<(&str, PathBuf)> = vec![
        ("FFmpeg", ffmpeg_path.clone()),
        ("LLM CLI", llama_cli.clone()),
        ("LLM 模型", llm_model.clone()),
        ("sample.mp4", sample_mp4.clone()),
    ];
    if is_sv {
        assets.push(("tools/sensevoice_runner.py", sv_runner.clone()));
        assets.push(("SenseVoice 模型", sv_model.clone()));
        assets.push(("SenseVoice 词表", sv_tokens.clone()));
        assets.push(("SenseVoice VAD", sv_vad.clone()));
    } else {
        assets.push(("Whisper CLI", whisper_cli.clone()));
        assets.push(("Whisper 模型", whisper_model.clone()));
    }
    let asset_refs: Vec<(&str, &Path)> = assets
        .iter()
        .map(|(name, path)| (*name, path.as_path()))
        .collect();
    if common::skip_heavy("test_full_pipeline_run", &asset_refs) {
        return;
    }

    // 资产齐备：以下断言与全链路执行全部保留（资产在 → 必须真跑，绝不当空壳）。
    assert!(ffmpeg_path.exists(), "FFmpeg 路径不存在: {ffmpeg_path:?}");
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

    // 2. 语音识别：按默认引擎二选一（SenseVoice / whisper.cpp）
    println!("\n[2/4] 语音识别...");
    let (segments, vad_sec) = if is_sv {
        let sensevoice = SenseVoiceEngine::new(
            sv_runner.clone(),
            sv_model.clone(),
            sv_tokens.clone(),
            sv_vad.clone(),
            8,
        );
        sensevoice
            .transcribe(&wav, Some("zh"), None, None, None)
            .expect("SenseVoice 转写失败")
    } else {
        let whisper =
            WhisperEngine::new(whisper_cli.to_string_lossy().as_ref(), &whisper_model, 4, 0);
        whisper
            .transcribe(&wav, Some("zh"), None, None, None)
            .expect("转写失败")
    };
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
    let llm = LLMEngine::new(llama_cli.to_string_lossy().as_ref(), &llm_model, 4096, 4);
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
