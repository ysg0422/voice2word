#[test]
fn test_system_monitor() {
    let mut monitor = voice2word::utils::SystemMonitor::new();
    std::thread::sleep(std::time::Duration::from_millis(200));
    let metrics = monitor.sample();
    println!("=== 资源监控测试结果 ===");
    println!("系统 CPU: {:.1}%", metrics.sys_cpu);
    println!("系统已用内存: {}", voice2word::app::ResourceMetrics::format_bytes(metrics.sys_mem_used));
    println!("系统总内存: {}", voice2word::app::ResourceMetrics::format_bytes(metrics.sys_mem_total));
    println!("进程标签: {}", metrics.proc_name);
    println!("进程 CPU: {:.1}%", metrics.proc_cpu);
    println!("进程内存: {}", voice2word::app::ResourceMetrics::format_bytes(metrics.proc_mem));
    println!("模型运行状态: {}", metrics.is_model_running);
    println!("========================");
    assert!(metrics.sys_mem_total > 0, "总内存应大于0");
}
