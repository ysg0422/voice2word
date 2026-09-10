use voice2word::core::{
    run_cpu_benchmark, HardwareInfo, InferenceProfile, PerformanceLevel, UserStrategy,
};

#[test]
fn test_hardware_detection_and_evaluation() {
    let hw = HardwareInfo::detect();
    println!("Detected CPU: {}", hw.cpu_brand);
    println!("Cores: {} / Threads: {}", hw.physical_cores, hw.logical_threads);
    println!("Total RAM: {}", hw.formatted_total_memory());
    println!("Available RAM: {}", hw.formatted_available_memory());
    println!("Inference Mode: {:?}", hw.inference_mode);

    assert!(!hw.cpu_brand.is_empty());
    assert!(hw.logical_threads >= 1);
    assert!(hw.total_memory_bytes > 0);

    let level = hw.evaluate_performance();
    println!("Evaluated Performance Level: {:?}", level);
}

#[test]
fn test_inference_strategy_decision_matrix() {
    let hw = HardwareInfo::detect();
    let level = hw.evaluate_performance();

    let speed_profile = InferenceProfile::decide(level, UserStrategy::Speed, &hw);
    let balanced_profile = InferenceProfile::decide(level, UserStrategy::Balanced, &hw);
    let quality_profile = InferenceProfile::decide(level, UserStrategy::Quality, &hw);

    println!("Speed Profile: {:?}", speed_profile);
    println!("Balanced Profile: {:?}", balanced_profile);
    println!("Quality Profile: {:?}", quality_profile);

    assert!(speed_profile.whisper_threads >= 2);
    assert!(balanced_profile.whisper_threads >= 2);
    assert!(quality_profile.whisper_threads >= 2);
    assert!(speed_profile.max_concurrency >= 1);
}

#[test]
fn test_cpu_benchmark() {
    let bench = run_cpu_benchmark();
    println!("Benchmark Elapsed: {} ms", bench.duration_ms);
    println!("Benchmark Score: {}", bench.score);
    println!("GFLOPS: {:.2}", bench.gflops_estimate);
    println!("Rating: {}", bench.throughput_rating);

    assert!(bench.duration_ms > 0);
    assert!(bench.score > 0);
    assert!(!bench.throughput_rating.is_empty());
}
