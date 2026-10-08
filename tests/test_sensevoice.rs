use std::path::PathBuf;
use std::sync::Arc;
use voice2word::engines::SenseVoiceEngine;
use voice2word::utils::AppConfig;

mod common;

#[test]
fn test_sensevoice_engine_basic() {
    let runner = AppConfig::resolve_path("tools/sensevoice_runner.py");
    let model = AppConfig::resolve_path("models/sensevoice/model.int8.onnx");
    let tokens = AppConfig::resolve_path("models/sensevoice/tokens.txt");
    let vad = AppConfig::resolve_path("models/sensevoice/silero_vad.onnx");

    println!("Runner: {:?}", runner);
    println!("Model: {:?}", model);
    println!("Tokens: {:?}", tokens);
    println!("VAD: {:?}", vad);

    // 优雅跳过：CI 或本机缺 SenseVoice 资产时打印 SKIP 并尽早 return。
    if common::skip_heavy(
        "test_sensevoice_engine_basic",
        &[
            ("tools/sensevoice_runner.py", &runner),
            ("models/sensevoice/model.int8.onnx", &model),
            ("models/sensevoice/tokens.txt", &tokens),
            ("models/sensevoice/silero_vad.onnx", &vad),
        ],
    ) {
        return;
    }

    // 资产齐备：断言与真实转录全部保留。
    assert!(runner.exists(), "tools/sensevoice_runner.py 必须存在");
    assert!(model.exists(), "models/sensevoice/model.int8.onnx 必须存在");
    assert!(tokens.exists(), "models/sensevoice/tokens.txt 必须存在");
    assert!(vad.exists(), "models/sensevoice/silero_vad.onnx 必须存在");

    let engine = SenseVoiceEngine::new(runner, model, tokens, vad, 4);
    assert!(engine.is_available(), "SenseVoice 引擎状态必须为可用");

    let sample_wav = PathBuf::from("resources/sample/test_speech.wav");
    if sample_wav.exists() {
        println!("开始 SenseVoice 文件转录测试: {:?}", sample_wav);
        let (segments, elapsed) = engine
            .transcribe(&sample_wav, Some("zh"), Some(4), None, None)
            .expect("SenseVoice 转写失败");

        println!(
            "SenseVoice 转录成功，耗时: {:.2}s, 片段数: {}",
            elapsed,
            segments.len()
        );
        for seg in &segments {
            println!(
                "  #{}: [{:.2} -> {:.2}] {}",
                seg.index, seg.start, seg.end, seg.text
            );
        }
        assert!(!segments.is_empty(), "SenseVoice 未能输出任何文本片段");
    } else {
        common::report_skip(
            "test_sensevoice_engine_basic 的文件转录段",
            &format!(
                "缺少 resources/sample/test_speech.wav（{}）",
                sample_wav.display()
            ),
        );
    }
}

#[tokio::test]
async fn test_sensevoice_pipeline_streaming() {
    let config = AppConfig::load_from_file("config.toml").expect("加载 config.toml 失败");
    let ffmpeg_path = AppConfig::resolve_path(&config.paths.ffmpeg);
    let whisper_cli = AppConfig::resolve_path(&config.paths.whisper_cli);
    let whisper_model = AppConfig::resolve_path(&config.paths.whisper_model);
    let llama_cli = AppConfig::resolve_path(&config.paths.llama_cli);
    let llm_model = AppConfig::resolve_path(&config.paths.llm_model);
    let runner = AppConfig::resolve_path("tools/sensevoice_runner.py");
    let model = AppConfig::resolve_path("models/sensevoice/model.int8.onnx");
    let tokens = AppConfig::resolve_path("models/sensevoice/tokens.txt");
    let vad = AppConfig::resolve_path("models/sensevoice/silero_vad.onnx");
    let sample_mp4 = PathBuf::from("resources/sample/sample.mp4");

    // 优雅跳过：整条 SenseVoice 流水线依赖 ffmpeg / whisper / llm / SenseVoice
    // 全套资产 + 样片；CI 或本机缺任一即打印 SKIP 并尽早 return。
    if common::skip_heavy(
        "test_sensevoice_pipeline_streaming",
        &[
            ("FFmpeg", &ffmpeg_path),
            ("Whisper CLI", &whisper_cli),
            ("Whisper 模型", &whisper_model),
            ("LLM CLI", &llama_cli),
            ("LLM 模型", &llm_model),
            ("tools/sensevoice_runner.py", &runner),
            ("models/sensevoice/model.int8.onnx", &model),
            ("models/sensevoice/tokens.txt", &tokens),
            ("models/sensevoice/silero_vad.onnx", &vad),
            ("sample.mp4", &sample_mp4),
        ],
    ) {
        return;
    }

    // 资产齐备：以下断言与流水线执行全部保留。
    assert!(sample_mp4.exists(), "sample.mp4 必须存在");

    let ffmpeg = Arc::new(voice2word::engines::FFmpegEngine::new(ffmpeg_path));

    let whisper = Arc::new(voice2word::engines::WhisperEngine::new(
        whisper_cli,
        whisper_model,
        4,
        0,
    ));

    let llm = Arc::new(voice2word::engines::LLMEngine::new(
        llama_cli,
        llm_model,
        config.pipeline.llm_ctx,
        config.pipeline.llm_threads,
    ));

    let sensevoice = Some(Arc::new(SenseVoiceEngine::new(
        runner, model, tokens, vad, 4,
    )));

    let pipeline = Arc::new(voice2word::core::TaskPipeline::new(
        ffmpeg, whisper, sensevoice, llm, None,
    ));

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

    tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            match event {
                voice2word::core::PipelineEvent::SegmentStream(seg) => {
                    println!(
                        "  [SenseVoice 实时出字] #{}: [{:.2}s -> {:.2}s] {}",
                        seg.index, seg.start, seg.end, seg.text
                    );
                }
                voice2word::core::PipelineEvent::Progress {
                    stage,
                    progress,
                    detail,
                } => {
                    println!("[{stage}] {:.1}% - {detail}", progress * 100.0);
                }
                voice2word::core::PipelineEvent::Finished(segs, metrics) => {
                    println!(
                        "Pipeline 完成! 总句数: {}, 总耗时: {:.2}s",
                        segs.len(),
                        metrics.total_elapsed_sec
                    );
                    println!("{}", metrics.format_summary_block());
                }
                voice2word::core::PipelineEvent::Error(err) => {
                    eprintln!("Pipeline 报错: {err}");
                }
                _ => {}
            }
        }
    });

    let model_override = Some(AppConfig::resolve_path("models/sensevoice/model.int8.onnx"));
    let segments = pipeline
        .run(
            sample_mp4,
            None,
            Some("zh".into()),
            "srt".into(),
            false,
            None,
            Some(4),
            model_override,
            0.0,
            tx,
        )
        .await
        .expect("SenseVoice 流水线执行失败");

    assert!(!segments.is_empty(), "流水线未产出任何字幕片段");
    println!(
        "SenseVoice 流水线测试圆满通过，共生成 {} 句字幕",
        segments.len()
    );
}
