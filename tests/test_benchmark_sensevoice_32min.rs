use std::path::PathBuf;
use std::sync::Arc;
use voice2word::engines::SenseVoiceEngine;
use voice2word::utils::AppConfig;

#[tokio::test]
async fn test_benchmark_sensevoice_32min() {
    let config = AppConfig::load_from_file("config.toml").expect("加载 config.toml 失败");
    let ffmpeg = Arc::new(voice2word::engines::FFmpegEngine::new(AppConfig::resolve_path(&config.paths.ffmpeg)));
    let whisper = Arc::new(voice2word::engines::WhisperEngine::new(
        AppConfig::resolve_path(&config.paths.whisper_cli),
        AppConfig::resolve_path(&config.paths.whisper_model),
        8,
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

    let sensevoice = Some(Arc::new(SenseVoiceEngine::new(runner, model, tokens, vad, 8)));
    let pipeline = Arc::new(voice2word::core::TaskPipeline::new(ffmpeg, whisper, sensevoice, llm, None));

    let video = PathBuf::from("testVideo/03.1.3概率不等式.mp4");
    assert!(video.exists(), "32分钟测试视频不存在");

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut first_word_time = None;
        let start = std::time::Instant::now();
        while let Some(event) = rx.recv().await {
            match event {
                voice2word::core::PipelineEvent::SegmentStream(seg) => {
                    if first_word_time.is_none() {
                        first_word_time = Some(start.elapsed());
                        println!("\n>>> [首屏体感] 首句转写出屏耗时: {:.2}s <<<", start.elapsed().as_secs_f64());
                        println!("    第一句内容: #{}: [{:.2}s -> {:.2}s] {}\n", seg.index, seg.start, seg.end, seg.text);
                    }
                    if seg.index % 50 == 0 {
                        println!("  [流式出字中] 第 {} 句: [{:.2}s -> {:.2}s] {}", seg.index, seg.start, seg.end, seg.text);
                    }
                }
                voice2word::core::PipelineEvent::Progress { stage, progress, detail } => {
                    if (progress * 100.0) as u32 % 20 == 0 {
                        println!("[{stage}] {:.1}% - {detail}", progress * 100.0);
                    }
                }
                voice2word::core::PipelineEvent::Finished(segs, metrics) => {
                    println!("\n========== 32 分钟长视频 SenseVoice 性能报告 ==========");
                    println!("总转录字幕行数: {}", segs.len());
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
    let srt_out = PathBuf::from("testVideo/03.1.3概率不等式_sensevoice.srt");
    let segments = pipeline
        .run(
            video,
            Some(srt_out),
            Some("zh".into()),
            "srt".into(),
            false,
            None,
            Some(8),
            model_override,
            0.0,
            tx,
        )
        .await
        .expect("SenseVoice 32 分钟转写执行失败");

    assert!(!segments.is_empty(), "未转写出任何字幕片段");
    println!("测试顺利完成！");
}
