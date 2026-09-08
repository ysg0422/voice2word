//! 系统与本地大模型硬件资源监控器 (CPU / 内存占用对比)

use std::time::Duration;
use sysinfo::{CpuRefreshKind, MemoryRefreshKind, ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System};
use tokio::sync::watch;

use crate::app::state::ResourceMetrics;

pub struct SystemMonitor {
    system: System,
    current_pid: Option<sysinfo::Pid>,
}

impl SystemMonitor {
    pub fn new() -> Self {
        let mut system = System::new_with_specifics(
            RefreshKind::new()
                .with_cpu(CpuRefreshKind::everything())
                .with_memory(MemoryRefreshKind::everything())
                .with_processes(ProcessRefreshKind::everything()),
        );
        system.refresh_cpu_all();
        system.refresh_memory();
        system.refresh_processes(ProcessesToUpdate::All);

        let current_pid = sysinfo::get_current_pid().ok();

        Self {
            system,
            current_pid,
        }
    }

    pub fn sample(&mut self) -> ResourceMetrics {
        self.system.refresh_cpu_all();
        self.system.refresh_memory();
        self.system.refresh_processes(ProcessesToUpdate::All);

        let sys_cpu = self.system.global_cpu_usage();
        let sys_mem_used = self.system.used_memory();
        let sys_mem_total = self.system.total_memory();

        let mut model_cpu: f32 = 0.0;
        let mut model_mem: u64 = 0;
        let mut active_model = String::new();
        let mut is_model_running = false;

        // 查找当前正在工作的模型或多媒体子进程
        for (_pid, process) in self.system.processes() {
            let name = process.name().to_string_lossy().to_lowercase();
            if name.contains("llama") || name.contains("whisper") || name.contains("ffmpeg") {
                is_model_running = true;
                model_cpu += process.cpu_usage();
                model_mem += process.memory();

                if active_model.is_empty() {
                    if name.contains("llama") {
                        active_model = "Qwen2.5 LLM (推理中)".to_string();
                    } else if name.contains("whisper") {
                        active_model = "Whisper ASR (转写中)".to_string();
                    } else {
                        active_model = "FFmpeg (提取音频)".to_string();
                    }
                }
            }
        }

        // 如果没有模型进程在跑，则统计主程序 voice2word 本身的开销
        if !is_model_running {
            if let Some(pid) = self.current_pid {
                if let Some(proc) = self.system.process(pid) {
                    model_cpu = proc.cpu_usage();
                    model_mem = proc.memory();
                }
            }
            active_model = "Voice2Word (待机)".to_string();
        }

        // 归一化进程 CPU 到整机百分比 (0% ~ 100%)，与系统总 CPU 保持同一维度
        let cpu_count = self.system.cpus().len().max(1) as f32;
        let proc_cpu_normalized = (model_cpu / cpu_count).clamp(0.0, 100.0);

        ResourceMetrics {
            sys_cpu,
            sys_mem_used,
            sys_mem_total,
            proc_name: active_model,
            proc_cpu: proc_cpu_normalized,
            proc_mem: model_mem,
            is_model_running,
        }
    }

    /// 启动异步后台监控线程，通过 watch channel 广播最新的指标
    pub fn spawn_background_monitor() -> watch::Receiver<ResourceMetrics> {
        let mut monitor = Self::new();
        let initial_metrics = monitor.sample();
        let (tx, rx) = watch::channel(initial_metrics);

        std::thread::Builder::new()
            .name("v2w-resource-monitor".to_string())
            .spawn(move || {
                loop {
                    std::thread::sleep(Duration::from_millis(1000));
                    let metrics = monitor.sample();
                    if tx.send(metrics).is_err() {
                        break; // 接收端关闭时退出线程
                    }
                }
            })
            .expect("启动系统资源监控线程失败");

        rx
    }
}
