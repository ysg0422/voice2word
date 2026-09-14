use std::path::PathBuf;
use std::sync::Arc;
use voice2word::engines::SenseVoiceEngine;
use voice2word::utils::AppConfig;

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

        println!("SenseVoice 转录成功，耗时: {:.2}s, 片段数: {}", elapsed, segments.len());
        for seg in &segments {
            println!("  #{}: [{:.2} -> {:.2}] {}", seg.index, seg.start, seg.end, seg.text);
        }
        assert!(!segments.is_empty(), "SenseVoice 未能输出任何文本片段");
    }
}

#[tokio::test]
async fn test_sensevoice_pipeline_streaming() {
    let config = AppConfig::load_from_file("config.toml").expect("加载 config.toml 失败");
    let ffmpeg = Arc::new(voice2word::engines::FFmpegEngine::new(AppConfig::resolve_path(&config.paths.ffmpeg)));

    let whisper = Arc::new(voice2word::engines::WhisperEngine::new(
        AppConfig::resolve_path(&config.paths.whisper_cli),
        AppConfig::resolve_path(&config.paths.whisper_model),
        4,
        0,
    ));

    let llm = Arc::new(voice2word::engines::LLMEngine::new(
        AppConfig::resolve_path(&config.paths.llama_cli),
        AppConfig::resolve_path(&config.paths.llm_model),
        config.pipeline.llm_ctx,
        config.pipeline.llm_threads,
    ));

    let runner = AppConfig::resolve_path("tools/sensevoice_runner.py");
    let model = AppConfig::resolve_path("models/sensevoice/model.int8.onnx");
    let tokens = AppConfig::resolve_path("models/sensevoice/tokens.txt");
    let vad = AppConfig::resolve_path("models/sensevoice/silero_vad.onnx");

    let sensevoice = Some(Arc::new(SenseVoiceEngine::new(runner, model, tokens, vad, 4)));

    let pipeline = Arc::new(voice2word::core::TaskPipeline::new(ffmpeg, whisper, sensevoice, llm, None));

    let sample_mp4 = PathBuf::from("resources/sample/sample.mp4");
    assert!(sample_mp4.exists(), "sample.mp4 必须存在");

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

    tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            match event {
                voice2word::core::PipelineEvent::SegmentStream(seg) => {
                    println!("  [SenseVoice 实时出字] #{}: [{:.2}s -> {:.2}s] {}", seg.index, seg.start, seg.end, seg.text);
                }
                voice2word::core::PipelineEvent::Progress { stage, progress, detail } => {
                    println!("[{stage}] {:.1}% - {detail}", progress * 100.0);
                }
                voice2word::core::PipelineEvent::Finished(segs, metrics) => {
                    println!("Pipeline 完成! 总句数: {}, 总耗时: {:.2}s", segs.len(), metrics.total_elapsed_sec);
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
            tx,
        )
        .await
        .expect("SenseVoice 流水线执行失败");

    assert!(!segments.is_empty(), "流水线未产出任何字幕片段");
    println!("SenseVoice 流水线测试圆满通过，共生成 {} 句字幕", segments.len());
}

