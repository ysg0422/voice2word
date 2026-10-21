//! 转写流水线阶段性能耗时统计与 Benchmark 基准数据模型

use crate::utils::time::format_duration_short;
use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};

/// 最近一次已经打印过的报告指纹（由 `take_print_slot` 维护）。
/// 初值为 `u64::MAX`（不可能与真实指纹相同的哨兵），保证第一次一定打印。
static LAST_PRINTED: AtomicU64 = AtomicU64::new(u64::MAX);

#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq)]
pub struct PipelinePerformanceMetrics {
    /// 媒体文件时长 (秒)
    pub video_duration: f64,
    /// VAD/字幕覆盖到的有声时间 (秒)。这是语音区间并集，不是 VAD 计算耗时。
    #[serde(default)]
    pub voiced_duration_sec: f64,
    /// 阶段 1: FFmpeg 提取音频耗时 (秒)
    pub ffmpeg_audio_sec: f64,
    /// 阶段 1 音频通道说明（如 "纯内存 PCM 匿名管道推流 (0 磁盘 I/O)" 或 "FFmpeg 抽取临时 WAV 文件"）
    #[serde(default)]
    pub audio_process_name: Option<String>,
    /// 阶段 2: Silero VAD 语音活性检测耗时 (秒)
    pub vad_sec: f64,
    /// 阶段 2 VAD 说明（如 "Silero VAD 毫秒级语音切片压实" 或 "SenseVoice 内嵌流式 VAD"）
    #[serde(default)]
    pub vad_engine_name: Option<String>,
    /// 阶段 3: 语音识别纯推理耗时 (秒)
    pub whisper_sec: f64,
    /// 阶段 3+: 置信度二段重解码耗时 (秒，未触发时为 0.0)
    #[serde(default)]
    pub rescue_sec: f64,
    /// 阶段 3+: 触发重解码救回的低置信窗口数量
    #[serde(default)]
    pub rescue_span_count: usize,
    /// 阶段 3 ASR 引擎详细说明（如 "SenseVoice-Small (非自回归 INT8 · CPU 8线程)" 或 "Whisper Turbo Q5 (ggml-large-v3-turbo-q5_0.bin)"）
    #[serde(default)]
    pub asr_engine_name: Option<String>,
    /// 阶段 4: 字幕润色/标点恢复耗时 (秒，未启用时为 0.0)
    pub qwen_sec: f64,
    /// 阶段 4 引擎说明（如 "SenseVoice 原生标点与 ITN (免二次后处理)" 或 "CT-Punc 极速标点 (INT8)" 或 "Qwen 字幕润色"）
    #[serde(default)]
    pub polish_engine_name: Option<String>,
    /// 阶段 4+: 说话人分离耗时 (秒，未启用时为 0.0)
    #[serde(default)]
    pub diarization_sec: f64,
    /// 阶段 5: 字幕写出与导出耗时 (秒)
    pub srt_export_sec: f64,
    /// 阶段 5 导出说明（如 "SRT 标准字幕写出"）
    #[serde(default)]
    pub export_name: Option<String>,
    /// 产出的字幕片段总数量
    #[serde(default)]
    pub segment_count: usize,
    /// 首字延迟 (秒)：从「进入语音识别阶段」到「引擎吐出第一段输出」的时间。
    ///
    /// 这一项把**子进程启动 + 模型加载**从「阶段 3」里单独拎出来：
    /// - SenseVoice / CT-Punc 是 Python 冷启动 + ONNX 会话创建（几百 MB 权重从磁盘读入）；
    /// - Whisper 是 whisper-cli 载入 ggml 权重 + Vulkan/CPU 后端初始化。
    ///
    /// 用户体感里「点了开始要等半天才开始出字」就是这一项；此前它被并进阶段 3 的
    /// 总时长，报告里看不出来，容易误判成「识别本身很慢」。
    #[serde(default)]
    pub asr_startup_sec: f64,
    /// 全流程总耗时 (秒)
    pub total_elapsed_sec: f64,
}

// `f64` 没有实现 `Hash`，而本类型需要可哈希（`fingerprint` 用 `DefaultHasher` 做
// 打印去重），因此手写实现：每个浮点字段按 `to_bits()` 参与哈希。
//
// 刻意从严：位型相同才算是同一份报告，于是 `0.0` 与 `-0.0`（以及不同位型的 NaN）
// 会算出不同指纹，尽管 `PartialEq` 判它们相等。这只影响「终端报告要不要去重」的
// 灵敏度——最坏情况是数值等价的两份报告多打印一次，不影响任何正确性。
impl Hash for PipelinePerformanceMetrics {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.video_duration.to_bits().hash(state);
        self.voiced_duration_sec.to_bits().hash(state);
        self.ffmpeg_audio_sec.to_bits().hash(state);
        self.audio_process_name.hash(state);
        self.vad_sec.to_bits().hash(state);
        self.vad_engine_name.hash(state);
        self.whisper_sec.to_bits().hash(state);
        self.rescue_sec.to_bits().hash(state);
        self.rescue_span_count.hash(state);
        self.asr_engine_name.hash(state);
        self.qwen_sec.to_bits().hash(state);
        self.polish_engine_name.hash(state);
        self.diarization_sec.to_bits().hash(state);
        self.srt_export_sec.to_bits().hash(state);
        self.export_name.hash(state);
        self.segment_count.hash(state);
        self.asr_startup_sec.to_bits().hash(state);
        self.total_elapsed_sec.to_bits().hash(state);
    }
}

impl PipelinePerformanceMetrics {
    /// 生成标准终端与文件详细性能报告
    pub fn format_summary_block(&self) -> String {
        let dur_str = format_duration_short(self.video_duration);
        let audio_desc =
            self.audio_process_name
                .as_deref()
                .unwrap_or(if self.ffmpeg_audio_sec == 0.0 {
                    "纯内存 PCM 匿名管道推流 (0 物理磁盘 I/O)"
                } else {
                    "FFmpeg 本地临时 WAV 提取"
                });
        let vad_desc = self
            .vad_engine_name
            .as_deref()
            .unwrap_or(if self.vad_sec == 0.0 {
                "内嵌流式 VAD 检测"
            } else {
                "Silero VAD 毫秒级语音切片压实"
            });
        let asr_desc = self
            .asr_engine_name
            .as_deref()
            .unwrap_or("Whisper 语音转写");
        let rescue_desc = if self.rescue_span_count > 0 {
            format!("{} 个低置信窗口已带回退重解码救回", self.rescue_span_count)
        } else {
            "未触发 (全程置信度良好或已关闭)".to_string()
        };
        let polish_desc = self
            .polish_engine_name
            .as_deref()
            .unwrap_or("标点/AI润色 (跳过)");
        let diarization_desc = if self.diarization_sec > 0.0 {
            "声学特征聚类 (基频/能量/过零率)"
        } else {
            "未启用"
        };
        let export_desc = self.export_name.as_deref().unwrap_or("字幕文件写出");

        let total = self.total_elapsed_sec.max(0.001);
        // 各阶段之和之外的「碎片时间」：阶段切换、optimize_segments 整理、
        // 阶段产物落盘、取消检查等。以前它完全不体现在报告里，于是
        // 「各阶段加起来 12s、总耗时 18s」时用户只能猜那 6 秒去哪了。
        let accounted = self.ffmpeg_audio_sec
            + self.vad_sec
            + self.whisper_sec
            + self.rescue_sec
            + self.qwen_sec
            + self.diarization_sec
            + self.srt_export_sec;
        let overhead_sec = (total - accounted).max(0.0);
        let overhead_pct = (overhead_sec / total * 100.0).clamp(0.0, 100.0);
        let startup_pct = (self.asr_startup_sec / total * 100.0).clamp(0.0, 100.0);
        // 只有确实测到才打这一行（断点续跑 / 复用产物时没有引擎启动）。
        let startup_line = if self.asr_startup_sec > 0.0 {
            format!(
                "          └ 其中 引擎启动（进程 + 模型加载）：{:>6.1} 秒 ({:>4.1}%)",
                self.asr_startup_sec, startup_pct
            )
        } else {
            String::new()
        };
        let audio_pct = (self.ffmpeg_audio_sec / total * 100.0).clamp(0.0, 100.0);
        let vad_pct = (self.vad_sec / total * 100.0).clamp(0.0, 100.0);
        let asr_pct = (self.whisper_sec / total * 100.0).clamp(0.0, 100.0);
        let rescue_pct = (self.rescue_sec / total * 100.0).clamp(0.0, 100.0);
        let polish_pct = (self.qwen_sec / total * 100.0).clamp(0.0, 100.0);
        let diarization_pct = (self.diarization_sec / total * 100.0).clamp(0.0, 100.0);
        let export_pct = (self.srt_export_sec / total * 100.0).clamp(0.0, 100.0);

        let speed_ratio = if total > 0.0 && self.video_duration > 0.0 {
            format!(
                "{:.1}x 实时速度 (耗时仅为媒体时长的 {:.1}%)",
                self.video_duration / total,
                (total / self.video_duration) * 100.0
            )
        } else {
            "毫秒级".to_string()
        };

        let seg_info = if self.segment_count > 0 {
            format!("{} 句", self.segment_count)
        } else {
            "-".to_string()
        };

        format!(
            r#"
======================= 全链路性能与耗时统计 =======================
媒体总时长：        {dur_str} ({:.1} 秒)
有声覆盖时长：      {:.1} 秒 ({:.1}%)
生成字幕句数：      {seg_info}
处理吞吐倍率：      {speed_ratio}

[阶段 1] 音频通道：    {:>6.1} 秒 ({:>4.1}%) | {audio_desc}
[阶段 2] 语音活性检测：{:>6.1} 秒 ({:>4.1}%) | {vad_desc}
[阶段 3] 语音转写识别：{:>6.1} 秒 ({:>4.1}%) | {asr_desc}
{startup_line}
[阶段 3+]置信度救场：  {:>6.1} 秒 ({:>4.1}%) | {rescue_desc}
[阶段 4] 标点与语法：  {:>6.1} 秒 ({:>4.1}%) | {polish_desc}
[阶段 4+]说话人分离：  {:>6.1} 秒 ({:>4.1}%) | {diarization_desc}
[阶段 5] 字幕导出写出：{:>6.1} 秒 ({:>4.1}%) | {export_desc}

未计入各阶段的开销：  {:>6.1} 秒 ({:>4.1}%) | 阶段切换 / 字幕整理 / 产物落盘
全流程总耗时：       {:>6.1} 秒
===================================================================
"#,
            self.video_duration,
            self.voiced_duration_sec,
            if self.video_duration > 0.0 {
                self.voiced_duration_sec / self.video_duration * 100.0
            } else {
                0.0
            },
            self.ffmpeg_audio_sec,
            audio_pct,
            self.vad_sec,
            vad_pct,
            self.whisper_sec,
            asr_pct,
            self.rescue_sec,
            rescue_pct,
            self.qwen_sec,
            polish_pct,
            self.diarization_sec,
            diarization_pct,
            self.srt_export_sec,
            export_pct,
            overhead_sec,
            overhead_pct,
            self.total_elapsed_sec,
        )
    }

    /// 报告指纹：对「报告里真正会被打印出来的字段」取哈希。
    ///
    /// 全是零值（默认构造的空指标，例如用户启动后立刻取消）时返回 `None`，
    /// 这类「什么都没发生」的报告不值得占用终端。
    fn fingerprint(&self) -> Option<u64> {
        let is_empty = self.video_duration == 0.0
            && self.voiced_duration_sec == 0.0
            && self.ffmpeg_audio_sec == 0.0
            && self.vad_sec == 0.0
            && self.whisper_sec == 0.0
            && self.rescue_sec == 0.0
            && self.rescue_span_count == 0
            && self.qwen_sec == 0.0
            && self.diarization_sec == 0.0
            && self.srt_export_sec == 0.0
            && self.segment_count == 0
            && self.total_elapsed_sec == 0.0;
        if is_empty {
            return None;
        }

        let mut hasher = DefaultHasher::new();
        self.hash(&mut hasher);
        Some(hasher.finish())
    }

    /// 取用「本次报告是否应该打印」的判定：若与上一次已打印的报告完全相同，
    /// 返回 `false`（并**保持**标志不变，因此同一份报告无论重复调用多少次都只有一次为真）。
    ///
    /// 为什么要去重：同一条消息会经由两条路径抵达终端——`tracing` 的日志 writer
    /// （`DualWriter`）已经把 `info!` 写进 stdout 与 logs/*.txt，而管线另外又
    /// `println!` 了一份。同一份多行报告因此在控制台出现两遍、日志文件里也占两份，
    /// 批量转写时刷屏。这里把两条路径合并判定：先到的打印、后到的丢弃，
    /// 无论调用顺序如何，始终只打印一次。
    pub fn take_print_slot(&self) -> bool {
        let Some(fingerprint) = self.fingerprint() else {
            return false;
        };
        // 不能写成 `compare_exchange(fingerprint, fingerprint, ...)`：期望值与新值相同时
        // 那次 store 是无操作，优化器会把它退化成纯 load，`LAST_PRINTED` 便永远停在哨兵上，
        // 每次调用都会被判成「新报告」而重复打印。用 `swap` 才能真正推进标志。
        LAST_PRINTED.swap(fingerprint, Ordering::SeqCst) != fingerprint
    }
}

#[cfg(test)]
mod tests {

    /// 首字延迟必须真的出现在报告里，且**作为阶段 3 的子集**呈现。
    ///
    /// 这条是用户的原始诉求：模型加载要等十几秒，但报告里看不到，
    /// 于是误以为「识别本身很慢」。
    #[test]
    fn summary_prints_engine_startup_under_stage3() {
        let m = PipelinePerformanceMetrics {
            video_duration: 60.0,
            voiced_duration_sec: 55.0,
            ffmpeg_audio_sec: 1.0,
            vad_sec: 0.5,
            whisper_sec: 12.0,    // 阶段 3 总时长
            asr_startup_sec: 9.5, // 其中 9.5s 是进程启动 + 模型加载
            qwen_sec: 1.0,
            srt_export_sec: 0.5,
            segment_count: 20,
            total_elapsed_sec: 16.0,
            ..Default::default()
        };
        let text = m.format_summary_block();
        assert!(
            text.contains("引擎启动"),
            "报告里必须有「引擎启动」一行，实得：\n{text}"
        );
        assert!(text.contains("9.5"), "要显示测到的启动秒数：\n{text}");
        assert!(
            text.contains("12.0"),
            "阶段 3 的数值不能被启动时间顶掉：\n{text}"
        );
    }

    /// 没有测到启动时间（断点续跑 / 复用产物）时，不该凭空打印一行 0.0。
    #[test]
    fn summary_omits_startup_line_when_not_measured() {
        let m = PipelinePerformanceMetrics {
            whisper_sec: 5.0,
            total_elapsed_sec: 5.2,
            ..Default::default()
        };
        assert!(!m.format_summary_block().contains("引擎启动"));
    }

    /// 各阶段之和与总耗时的差额必须被显式交代，不能留成「莫名消失的 N 秒」。
    #[test]
    fn summary_accounts_for_time_outside_stages() {
        let m = PipelinePerformanceMetrics {
            ffmpeg_audio_sec: 1.0,
            vad_sec: 1.0,
            whisper_sec: 4.0,
            qwen_sec: 1.0,
            srt_export_sec: 1.0,
            total_elapsed_sec: 10.0, // 各阶段合计 8.0 → 差 2.0
            ..Default::default()
        };
        let text = m.format_summary_block();
        assert!(
            text.contains("未计入各阶段的开销"),
            "必须交代阶段之外的耗时：\n{text}"
        );
        assert!(text.contains("2.0"), "差额应打印为 2.0 秒：\n{text}");
    }

    use super::*;

    /// 打印名额的三种情形**必须放在同一个用例里**：`LAST_PRINTED` 是进程内全局状态，
    /// 拆成多个 `#[test]` 会在 `cargo test --lib` 的并行执行下互相踩（一个用例的
    /// CAS 会改变另一个用例看到的「上一次指纹」），断言随之偶发失败。
    #[test]
    fn print_slot_dedups_identical_reports_only() {
        // 1) 同一份报告：`println!` 与 `tracing` 两条路径共用一次名额，第二次必须被丢弃
        let dup = PipelinePerformanceMetrics {
            video_duration: 120.0,
            total_elapsed_sec: 12.0,
            segment_count: 8,
            ..Default::default()
        };
        assert!(dup.take_print_slot(), "首次打印必须放行");
        assert!(!dup.take_print_slot(), "完全相同的报告第二次必须被丢弃");
        assert!(!dup.take_print_slot(), "重复调用再多也不得放行");

        // 2) 内容不同的报告（不同时长 / 片段数）不应互相压制
        let first = PipelinePerformanceMetrics {
            video_duration: 100.0,
            total_elapsed_sec: 10.0,
            segment_count: 5,
            ..Default::default()
        };
        let second = PipelinePerformanceMetrics {
            video_duration: 200.0,
            total_elapsed_sec: 20.0,
            segment_count: 9,
            ..Default::default()
        };
        assert!(first.take_print_slot());
        assert!(second.take_print_slot());

        // 3) 全零（空）指标不占名额：用户启动后立刻取消时不该刷一屏空报告。
        // 只读判定，不触碰全局状态。
        assert!(!PipelinePerformanceMetrics::default().take_print_slot());
    }
}
