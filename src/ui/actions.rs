//! MainWindow 异步与后台业务动作逻辑

use gpui::prelude::*;
use std::path::PathBuf;
use tokio::sync::mpsc;
use tracing::info;

use super::types::{BatchSummary, CompletionDialogInfo};
use super::{EditorExportFormat, MainWindow};
use crate::app::state::{ProcessStatus, QueueState, WorkspaceTab};
use crate::core::PipelineEvent;
use crate::subtitle::writer::export_spec_for;
use crate::subtitle::Segment;
use crate::subtitle::SubtitleWriter;

/// 导入字幕成功后的提示文案。
///
/// 抽成自由函数而不是闭包里的内联代码：`this.update(...)` 的闭包已经可变借用了
/// `MainWindow`，在其中再调 `self.` 的方法会借用冲突；把「纯字符串拼装」拎出来
/// 就没有这个问题，也顺带让这段文案可读。
///
/// TXT 分支必须显式提醒时间轴是合成的——用户很可能接着导出 SRT，而那份时间
/// 完全不是真的。
fn self_notice_import(this: &mut MainWindow, stem: &str, count: usize, is_txt: bool) {
    this.notice = Some(if is_txt {
        format!(
            "已导入「{stem}」{count} 句；纯文本没有时间信息，已按字数合成占位时间轴，导出前请核对"
        )
    } else {
        format!("已导入「{stem}」{count} 句字幕")
    });
}

/// `EditorExportFormat`（剪辑台的状态枚举）与配置字符串（`state.output_format`）
/// 之间的唯一桥梁：返回 `writer::write_in_place` / `export_spec_for` 认的规范格式名。
///
/// 之所以只映射到**格式字符串**、扩展名与过滤器名交给
/// [`export_spec_for`] 再算一次：这几项字面量原本在 `perform_editor_export`
/// 里被内联重复了一遍，两份表一旦漂移就会出现「文件叫 `.ttml`、过滤器却写
/// 别的格式」。`src/ui/mod.rs` 的枚举定义不动，此处只做「枚举 -> 字符串」，
/// 扩展名/过滤器名仍只有 `writer.rs` 一个来源。
///
/// 剪映（含导出到文件夹）/ FCPXML / Premiere XML 不走这条单文件链路
/// （整个草稿目录或专用 exporter），由 `perform_editor_export` 的 `match`
/// 提前分流，不会走到这里。
fn editor_export_format_string(fmt: EditorExportFormat) -> &'static str {
    match fmt {
        EditorExportFormat::Srt => "srt",
        EditorExportFormat::Ass => "ass",
        EditorExportFormat::Txt => "txt",
        EditorExportFormat::Vtt => "vtt",
        EditorExportFormat::Json => "json",
        EditorExportFormat::EbuTtD => "ttml",
        EditorExportFormat::NetflixTtal => "ttal",
        // 上面那几个是唯一会走到单文件导出的分支；其余三档在上层已分流。
        // 真被漏到这里时给最保守的 srt（writer 一定写得出来的格式），
        // 也比 panic 掉整个 UI 线程好。
        _ => "srt",
    }
}

/// 支持导入的音视频扩展名。文件对话框、拖放、批量队列三处共用同一份，
/// 避免某个入口悄悄漏掉一种格式。
pub(crate) const MEDIA_EXTS: [&str; 10] = [
    "mp4", "mkv", "mov", "avi", "flv", "webm", "mp3", "wav", "flac", "m4a",
];

/// 应有译文的句数：**源文非空**的句子。
///
/// 空源句（静音段、纯空白行）没有任何可译内容，引擎也永远不会给它译文
/// （引擎挑待译句的口径就是 `!seg.translate_source().trim().is_empty()`）。
/// 若把它们算进分母，一次成功的整片翻译也会永远差几句，界面只能显示
/// 「部分完成」——分母必须与引擎的可译口径一致。
pub(crate) fn expected_translation_count(segments: &[Segment]) -> usize {
    segments
        .iter()
        .filter(|s| !s.translate_source().trim().is_empty())
        .count()
}

/// 当前目标语言下**实际译出**的句数：非空译文（`has_translation`）且语言匹配。
///
/// 两个条件缺一不可：只要译文是空串/纯空白（引擎解析失败时可能写入，照样带
/// `translation_lang`），就不算「译出」——这正是界面「已全部翻译」不能骗人的关键。
pub(crate) fn translated_out_count(segments: &[Segment], target: &str) -> usize {
    segments
        .iter()
        .filter(|s| s.has_translation() && s.translation_matches(target))
        .count()
}

/// 翻译收尾的统计口径（纯函数，便于单测）：`(实际译出句数, 应有译文句数)`。
///
/// 分子只认**目标语言 + 非空译文**（空串/纯空白不算），分母只算源文非空的句子。
pub(crate) fn translation_coverage(segments: &[Segment], target: &str) -> (usize, usize) {
    let done = translated_out_count(segments, target);
    (done, expected_translation_count(segments))
}

impl MainWindow {
    /// 裁剪视频库的界面缓存，只保留仍存在于列表里的条目。
    ///
    /// `library_thumbs`（首帧缩略图路径）与 `library_sizes`（文件大小文本）
    /// 都按 `task_id` 累积，而 `recent_tasks` 只保留最近 50 条。用户长期
    /// 使用（不断导入/删除）后，这两个 map 会一直涨且全是查不到的僵尸条目。
    /// 在每次刷新列表后统一按当前列表裁剪，是最省心的一处收口。
    pub(crate) fn prune_library_caches(&mut self) {
        let live: std::collections::HashSet<i64> =
            self.state.recent_tasks.iter().map(|t| t.id).collect();
        self.library_thumbs.retain(|id, _| live.contains(id));
        self.library_sizes.retain(|id, _| live.contains(id));
        // 选中集同步裁掉已不存在的任务：删除/刷新后不该再留着查不到的 id。
        self.library_selected.retain(|id| live.contains(id));
    }

    /// 把视频库里勾选的多个任务一次性导出到同一目录。
    ///
    /// 与单条「导出字幕」同源：字幕按 id 现取（列表记录不含正文），导出内容模式
    /// 沿用导出栏的「原文 / 仅译文 / 双语」，导出格式沿用配置抽屉的
    /// 「字幕输出格式」（经 [`export_spec_for`]，未知值回落 srt）。逐条写盘，
    /// 失败的记下来一次性汇总提示，不让一条坏数据中断整批。
    pub(crate) fn export_selected_library_tasks(&mut self, cx: &mut Context<Self>) {
        if self.library_export_busy {
            return;
        }
        // 按列表顺序取出选中项（保证导出提示与视觉顺序一致）
        let picked: Vec<(i64, String)> = self
            .state
            .recent_tasks
            .iter()
            .filter(|t| self.library_selected.contains(&t.id))
            .map(|t| (t.id, t.file_name.clone()))
            .collect();
        if picked.is_empty() {
            return;
        }

        let export_mode = self.state.export_mode_from_config();
        // 格式在派发前就定下来：整批共用同一份 spec（同一扩展名、同一写出格式），
        // 与单条「导出字幕」走的是同一个 helper，不再各写一份字面量。
        let (fmt_ext, _fmt_label) = export_spec_for(&self.state.output_format);
        // 样式同样在派发前 clone 出来（与剪辑台 `perform_editor_export` 同法）：
        // 后台闭包是 `'static` 的，拿不到 `this`，必须先复制一份。此前批量导出
        // 调用的是无样式的 `write_to_file_with_mode`，用户在配置里把单行最大字数
        // 调小后，库里批量导出的字幕依旧不折行（中→英译文本会超屏），
        // 与剪辑台逐条导出的结果不一致。
        let style = self.state.config.subtitle_style.clone();
        // 文件名模板与日期同样要在派发前抽出来：`background_executor().spawn` 的
        // 闭包必须 `Send`，闭包里读 `self.state.config` 会把 `self` 拖进去。
        let name_template = self.state.config.ui.export_name_template.clone();
        let today = chrono::Local::now().format("%Y%m%d").to_string();
        let db = self.state.db.clone();
        self.library_export_busy = true;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let folder = rfd::AsyncFileDialog::new()
                .set_title("选择批量导出目录")
                .pick_folder()
                .await;
            let Some(folder_handle) = folder else {
                // 用户取消：复位忙状态，不弹提示
                let _ = this.update(cx, |this, cx| {
                    this.library_export_busy = false;
                    cx.notify();
                });
                return;
            };
            let dir = folder_handle.path().to_path_buf();
            // 后台闭包与收尾提示都要用 dir：各持一份，避免 move 冲突
            let dir_for_write = dir.clone();

            let result = cx
                .background_executor()
                .spawn(async move {
                    let mut ok = 0usize;
                    let mut failed: Vec<String> = Vec::new();
                    for (id, name) in &picked {
                        let segs = match db.load_task_segments(*id) {
                            Ok(s) if !s.is_empty() => s,
                            Ok(_) => {
                                failed.push(format!("{name}（无字幕内容）"));
                                continue;
                            }
                            Err(e) => {
                                failed.push(format!("{name}（读取失败: {e}）"));
                                continue;
                            }
                        };
                        // 文件名沿用工程名，剥掉原扩展名再补上所选格式的扩展名，
                        // 避免「xx.mp4.srt」。此前扩展名与写出格式都硬编码为
                        // "srt"，用户在配置里选了 JSON，批量导出却仍是 SRT 文件。
                        let stem = std::path::Path::new(name)
                            .file_stem()
                            .map(|s| s.to_string_lossy().to_string())
                            .unwrap_or_else(|| format!("task_{id}"));
                        // 文件名走统一模板：库批量导出与剪辑台单条导出必须给出
                        // 同一套命名规则，否则同一个配置在两个入口行为不同。
                        let fname = crate::subtitle::writer::export_file_name(
                            &name_template,
                            &stem,
                            fmt_ext,
                            &today,
                        );
                        let out = dir_for_write.join(&fname);
                        // 走带样式的入口：srt / vtt / ass 按 `max_chars_per_line` 折行。
                        // 其余格式（json / ttml / ttal / txt）在
                        // `write_to_file_with_style` 内部**原样回落**到
                        // `write_to_file_with_mode`，不会因为这次接线而报错或改变字节。
                        match SubtitleWriter::write_to_file_with_style(
                            &segs,
                            &out,
                            fmt_ext,
                            export_mode,
                            &style,
                        ) {
                            Ok(()) => ok += 1,
                            Err(e) => failed.push(format!("{name}（写出失败: {e}）")),
                        }
                    }
                    (ok, failed)
                })
                .await;

            let _ = this.update(cx, |this, cx| {
                this.library_export_busy = false;
                let (ok, failed) = result;
                if failed.is_empty() {
                    this.notice = Some(format!("已导出 {ok} 份字幕到 {}", dir.display()));
                } else {
                    let preview = failed
                        .iter()
                        .take(3)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join("；");
                    let more = if failed.len() > 3 {
                        format!("等 {} 项", failed.len())
                    } else {
                        String::new()
                    };
                    this.notice = Some(format!(
                        "批量导出完成：成功 {ok} 份，失败 {} 项（{preview}{more}）",
                        failed.len()
                    ));
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// CPU 机器强制走 H.264 代理；有 GPU 默认原片，用户仍可手动打开。
    pub(crate) fn ensure_preview_proxy(&mut self, cx: &mut Context<Self>) {
        self.probe_video_dimensions(cx);
        if !self.state.proxy_enabled {
            self.state.preview_source = self.state.selected_file.clone();
            self.state.proxy_busy = false;
            return;
        }
        let Some(source) = self.state.selected_file.clone() else {
            return;
        };
        if self.state.proxy_busy {
            return;
        }
        let cpu = !self.state.hardware.use_gpu_pipeline();
        let height = self.state.proxy_manager.preview_height(&source, cpu);
        if let Some(existing) = self.state.proxy_manager.existing_proxy(&source, height) {
            self.state.preview_source = Some(existing);
            self.state.proxy_busy = false;
            return;
        }
        self.state.proxy_busy = true;
        let proxy = self.state.proxy_manager.clone();
        info!(path = %source.display(), height, "后台生成 H.264 预览代理");
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { proxy.ensure_proxy(&source, height) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.state.proxy_busy = false;
                match result {
                    Ok(path) => {
                        this.state.preview_source = Some(path);
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, "代理生成失败，预览回退原片");
                        this.state.preview_source = this.state.selected_file.clone();
                    }
                }
                this.trigger_extract_frame(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// 打开系统原生文件对话框选择音视频（异步非阻塞，杜绝 Win32 模态循环导致的 GPUI RefCell 重入借用冲突）
    pub(crate) fn choose_file(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let handle = rfd::AsyncFileDialog::new()
                .add_filter(
                    "音视频文件",
                    &[
                        "mp4", "mkv", "mov", "avi", "flv", "webm", "mp3", "wav", "flac", "m4a",
                    ],
                )
                .pick_file()
                .await;
            if let Some(file_handle) = handle {
                let file = file_handle.path().to_path_buf();
                let _ = this.update(cx, |this, cx| {
                    this.adopt_media_file(file, cx);
                });
            }
        })
        .detach();
    }

    /// 删除某个可下载组件的本地文件（「模型与组件」卡的「删除」按钮走这里）。
    ///
    /// 删除逻辑**全部**委托给 `utils::model_download::delete_item_file`：
    /// 那边是唯一的路径解析 + 安全校验处（拒绝压缩包类组件、拒绝目录、拒绝用户
    /// 自编译构建），这里只负责把结果翻译成界面反馈，绝不在 UI 层再拼一遍路径——
    /// 两份实现一旦漂移，就会出现「界面允许删、底层拒绝」或更糟的「删错东西」。
    ///
    /// 反馈落在 `download_status_msg`（模型卡里就地显示，成功/失败/中性三色），
    /// 而不是 `state.status`：后者是红色横幅、且只在转写页可见，用户在性能页
    /// 点删除根本看不到。
    pub(crate) fn delete_model_file(&mut self, item_id: &str, cx: &mut Context<Self>) {
        let label = crate::utils::model_download::item_by_id(item_id)
            .map(|i| i.label)
            .unwrap_or(item_id);
        match crate::utils::model_download::delete_item_file(item_id, &self.state.config) {
            Ok(Some(path)) => {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| path.display().to_string());
                self.state.download_status_msg = format!("已删除 {name}，可重新下载");
                // 删完立刻重扫就位状态：徽标要从「已就位」翻回「下载」，
                // 否则用户会以为没删掉（卡片还显示已就位）。
                self.state.refresh_model_presence();
            }
            Ok(None) => {
                self.state.download_status_msg = format!("{label}：磁盘上没有对应文件，无需删除");
                self.state.refresh_model_presence();
            }
            Err(e) => {
                self.state.download_status_msg = format!("删除失败：{e}");
            }
        }
        cx.notify();
    }

    /// 一键备份数据目录（库 + 配置）到 `backups/` 下的时间戳归档。
    ///
    /// 为什么要有它：整个工程库就是一个 `voice2word.db`，误删或想换机器时用户
    /// 手上没有任何快照；而**运行中**手工复制这个文件是不安全的——SQLite 的 WAL
    /// 是独立文件，只拷 `.db` 会丢掉最近几次编辑。备份走 `utils::backup`
    /// （连 `-wal`/`-shm` 一起打包），把「安全的复制方式」固化成一次点击。
    ///
    /// 全程在后台线程做（打包几百 MB 的库会阻塞 UI）；完成后把归档路径写进
    /// `download_status_msg` 并**在资源管理器里选中它**，让用户立刻看到产物在哪。
    pub(crate) fn backup_data(&mut self, cx: &mut Context<Self>) {
        if self.state.is_downloading {
            return;
        }
        let root = crate::utils::AppConfig::app_root_dir();
        let name = crate::utils::backup::backup_file_name(chrono::Local::now());
        let dest = root.join("backups").join(&name);
        self.state.download_status_msg = "正在备份数据…".to_string();
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { crate::utils::backup::create_backup(&root, &dest) })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(outcome) => {
                        info!("数据备份完成: {}", outcome.archive.display());
                        this.state.download_status_msg = format!(
                            "已备份 {} 个文件（{}）到 backups/",
                            outcome.included.len(),
                            crate::utils::model_download::human_size(outcome.bytes)
                        );
                        // 打开资源管理器并选中归档：用户下一步多半就是把它拷走。
                        let _ = std::process::Command::new("explorer")
                            .arg(format!("/select,{}", outcome.archive.display()))
                            .spawn();
                    }
                    Err(e) => {
                        this.state.download_status_msg = format!("备份失败：{e}");
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 选择一份备份归档并（在二次确认后）恢复。
    ///
    /// 恢复会**写回** `voice2word.db` 等文件，因此先弹确认框；确认后才真正执行
    /// （走 [`MainWindow::restore_data_backup`]）。底层 `utils::backup::restore_backup`
    /// 本身拒绝覆盖已存在的文件——这是有意的双保险：即便用户误点确认，也不会
    /// 悄悄把当前工程库换掉；界面会把「哪些文件因为已存在而被保留」如实报出来。
    pub(crate) fn choose_backup_to_restore(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let handle = rfd::AsyncFileDialog::new()
                .set_title("选择要恢复的备份归档")
                .add_filter("Voice2Word 备份", &["zip"])
                .pick_file()
                .await;
            let Some(handle) = handle else { return };
            let path = handle.path().to_path_buf();
            let _ = this.update(cx, |this, cx| {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| path.display().to_string());
                this.confirm_dialog = Some(crate::ui::types::ConfirmDialogInfo {
                    title: "从备份恢复？".to_string(),
                    message: format!(
                        "将用「{name}」里的内容恢复数据目录。为保护你当前的工程，**已存在的同名文件不会被覆盖**——\
                         若确实要用备份替换，请先手动把现有 voice2word.db / config.toml 改名。"
                    ),
                    confirm_label: "恢复".to_string(),
                    danger: true,
                    action: crate::ui::types::ConfirmAction::RestoreBackup(path),
                });
                cx.notify();
            });
        })
        .detach();
    }

    /// 执行备份恢复（确认后调用）。结果同样落在 `download_status_msg`。
    pub(crate) fn restore_data_backup(
        &mut self,
        archive: &std::path::Path,
        cx: &mut Context<Self>,
    ) {
        let root = crate::utils::AppConfig::app_root_dir();
        match crate::utils::backup::restore_backup(archive, &root) {
            Ok(outcome) => {
                let mut msg = if outcome.restored.is_empty() {
                    "没有可恢复的文件".to_string()
                } else {
                    format!("已恢复 {} 个文件", outcome.restored.len())
                };
                if !outcome.skipped_existing.is_empty() {
                    // 这条提示是恢复功能的**核心安全语义**：用户以为「恢复=替换」，
                    // 实际是「只补缺失」。不说清他会以为恢复失败或数据没变。
                    msg.push_str(&format!(
                        "；{} 已存在，已保留现有版本（如需替换请先手动改名）",
                        outcome.skipped_existing.join("、")
                    ));
                }
                self.state.download_status_msg = msg;
                // 恢复的是库与配置：下次启动才读，这里只刷新界面上的就位/库视图。
                self.state.refresh_model_presence();
            }
            Err(e) => {
                self.state.download_status_msg = format!("恢复失败：{e}");
            }
        }
        cx.notify();
    }

    /// 打开模型/工具的落地根目录（`models/` 与 `tools/` 所在的目录）。    /// 打开模型/工具的落地根目录（`models/` 与 `tools/` 所在的目录）。
    ///
    /// 「打开目录」是为了兜住 `delete_item_file` 明确不肯删的那一类：压缩包组件
    /// （whisper-cli / llama.cpp）解压后铺在 `tools/` 下、与共享 DLL 混在一起，
    /// 程序没法安全地替用户删。与其让用户对着提示去手敲路径，不如给一个按钮直接
    /// 把目录甩到资源管理器里。
    /// 导出**质检复核表**（CSV / Markdown），供人工复核或交给他人。
    ///
    /// # 为什么需要它
    ///
    /// 质检结论此前只活在界面里：复核的人只能在剪辑台上一个个点，也没法把「哪些句
    /// 有问题」交给别人。复核表把结论变成 Excel 能开、工单能贴的东西。
    ///
    /// 走系统「保存文件」对话框而不是固定路径：复核表是给**人**用的交付物，
    /// 落点由用户决定（他要发给谁就放哪）。
    pub(crate) fn export_review_sheet(
        &mut self,
        format: crate::subtitle::qc::QcFormat,
        cx: &mut Context<Self>,
    ) {
        use crate::subtitle::segment::quality_report;
        if self.state.segments.is_empty() {
            return;
        }
        // 与转写页质检卡同一份判据（阈值取自配置、术语表取自缓存），保证导出的
        // 数字与界面上看到的一致——两处各算一遍必然出现「界面 3 句、导出 5 句」。
        let check_untranslated = self.state.translated_count() > 0;
        let threshold = self.state.config.pipeline.whisper_low_confidence;
        let mut report = quality_report(&self.state.segments, &[], threshold, check_untranslated);
        report.glossary_violations = self.cached_glossary_violations();
        let flagged = report.all_issues().len();
        let total = self.state.segments.len();
        let sheet = crate::subtitle::qc::render_review_sheet(&self.state.segments, &report, format);
        let stem = self
            .state
            .selected_file
            .as_ref()
            .and_then(|p| p.file_stem())
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "subtitle".to_string());
        let file_name = format!(
            "{}.{}",
            crate::subtitle::qc::review_file_stem(&stem),
            format.extension()
        );

        // 空表也给导出：用户点了按钮就该拿到文件（「没有问题」本身是结论），
        // 但要在提示里说清有几行，免得他以为导出漏了。
        self.notice = Some(format!("已导出质检表（{flagged} 行待复核，共 {total} 句）"));
        self.export_text_content(cx, file_name, sheet, format.label());
    }

    /// 检查视频库里的重复记录，并把结果作为中性提示告诉用户。
    ///
    /// # 为什么只提示、不自动清理
    ///
    /// 自动删掉「重复」是危险动作：指纹是**采样**哈希（见 `utils::fingerprint` 的
    /// 模块文档），理论上不同的文件可能撞上；更要紧的是用户可能刻意保留同内容的两条
    /// 记录（不同字幕版本、不同目标语言）。所以这里只回答「哪些是同一份媒体」，
    /// 删不删、删哪条由用户决定——`suggested_keeper` 给出建议（保留最早那条，
    /// 它可能已经人工校对过），但按钮不会替他按下。
    pub(crate) fn report_library_duplicates(&mut self, cx: &mut Context<Self>) {
        use crate::utils::duplicates::{describe, group_by_fingerprint, suggested_removals};
        let groups = group_by_fingerprint(&self.state.recent_tasks);
        let mut msg = describe(&groups);
        if let Some(first) = groups.first() {
            // 附一个具体例子：只说「有 3 组重复」用户不知道从哪下手。
            let sample: Vec<String> = first.names.clone();
            msg.push_str(&format!("；例如「{}」", sample.join("」与「")));
            let removals = suggested_removals(first).len();
            if removals > 0 {
                msg.push_str(&format!("（该组可清理 {removals} 条）"));
            }
        }
        // 落在 `notice`（中性）而不是 `status`（红色错误）：重复不是故障，
        // 是「你可能想清理一下」的信息。
        self.notice = Some(msg);
        cx.notify();
    }

    /// 检查是否有新版本（用户显式点击才发请求，不轮询）。
    ///
    /// # 为什么不做成自动检查
    ///
    /// 启动即发一个出站请求，对「完全本地运行」是这个项目的核心卖点之一的工具来说
    /// 是背道而驰的；而且 GitHub 未认证 API 有速率限制，自动轮询很容易把自己查 403。
    /// 所以这里只在用户点「检查更新」时请求一次，结果缓存进会话状态。
    ///
    /// 只**报告**、不下载也不替换 exe：静默替换正在运行的可执行文件是能把安装搞死
    /// 的操作，那属于单独的、更高风险的一步（见 `utils::update_check` 的模块文档）。
    pub(crate) fn check_for_update(&mut self, cx: &mut Context<Self>) {
        if self.state.update_check_busy {
            return;
        }
        self.state.update_check_busy = true;
        self.state.download_status_msg = "正在检查更新…".to_string();
        self.state.update_download_status = Some("正在检查更新…".to_string());
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { crate::utils::update_check::fetch_latest_release(15) })
                .await;
            let _ = this.update(cx, |this, cx| {
                use crate::utils::update_check::{current_version, describe, is_newer};
                this.state.update_check_busy = false;
                match result {
                    Ok(latest) => {
                        let current = current_version();
                        let newer = is_newer(&latest.version, current);
                        let text = describe(current, &latest.version, newer);
                        this.state.download_status_msg = text.clone();
                        this.state.update_check_result = Some((newer, text.clone(), latest.page_url));
                        this.state.update_download_target = Some((
                            latest.download_url,
                            latest.asset_name,
                            latest.asset_size,
                        ));
                        this.state.update_download_status = Some(text);
                    }
                    Err(e) => {
                        let err_msg = format!("检查更新失败：{e}");
                        this.state.download_status_msg = err_msg.clone();
                        this.state.update_download_status = Some(err_msg);
                        this.state.update_check_result = None;
                        this.state.update_download_target = None;
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 应用内直接下载最新版本（支持国内加速镜像，不跳转网页）
    pub(crate) fn download_app_update(&mut self, cx: &mut Context<Self>) {
        if self.state.update_download_busy {
            return;
        }
        let Some((url, name, _)) = self.state.update_download_target.clone() else {
            return;
        };
        if url.trim().is_empty() {
            return;
        }

        self.state.update_download_busy = true;
        self.state.update_download_progress = 0.0;
        self.state.update_download_status = Some("正在连接更新镜像…".to_string());
        self.update_download_cancel.store(false, std::sync::atomic::Ordering::Relaxed);
        let cancel = self.update_download_cancel.clone();
        cx.notify();

        cx.spawn(async move |this, cx| {
            let res = cx
                .background_executor()
                .spawn(async move {
                    crate::utils::update_check::download_update_file(
                        &url,
                        &name,
                        &cancel,
                        |_frac, _written, _total| {},
                    )
                })
                .await;

            let _ = this.update(cx, |this, cx| {
                this.state.update_download_busy = false;
                match res {
                    Ok(path) => {
                        this.state.update_download_progress = 1.0;
                        let msg = format!("新版本已下载就绪：{}", path.display());
                        this.state.update_download_status = Some(msg);
                        this.state.update_downloaded_path = Some(path);
                    }
                    Err(e) => {
                        this.state.update_download_status = Some(format!("下载失败：{e}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 打开已下载的新版文件所在文件夹
    pub(crate) fn open_downloaded_update(&mut self, _cx: &mut Context<Self>) {
        if let Some(path) = &self.state.update_downloaded_path {
            #[cfg(target_os = "windows")]
            {
                let _ = std::process::Command::new("explorer")
                    .arg(format!("/select,{}", path.display()))
                    .spawn();
            }
            #[cfg(not(target_os = "windows"))]
            {
                if let Some(p) = path.parent() {
                    let _ = open::that(p);
                }
            }
        }
    }

    /// 在浏览器里打开最新版本的下载页（`update_check_result` 里有 URL）。
    pub(crate) fn open_release_page(&mut self, cx: &mut Context<Self>) {
        let Some((_, _, url)) = self.state.update_check_result.clone() else {
            return;
        };
        if url.trim().is_empty() {
            return;
        }
        let _ = open::that(&url);
        cx.notify();
    }

    /// 导出**编辑日志**（本次会话改了哪些句、改前改后是什么）。
    ///
    /// # 为什么需要它
    ///
    /// 撤销栈解决的是「退回去」，解决不了「改了什么」：它只有 50 层、换文档即清空、
    /// 退出即丢。用户交片给客户、或两周后回头看自己的工程，都需要能查到「这句当时
    /// 是什么、谁改的、什么时候」。
    ///
    /// 展示前先 `coalesced(60)`：文本编辑逐键记一条，不折叠的话单个句子就能刷满
    /// 整张表。**导出的是折叠后的视图**，折叠只是展示取舍，原始记录仍在内存里。
    pub(crate) fn export_edit_log(
        &mut self,
        format: crate::subtitle::qc::QcFormat,
        cx: &mut Context<Self>,
    ) {
        use crate::subtitle::audit::{render_audit, DEFAULT_COALESCE_SECS};
        let log = self.state.edit_log.coalesced(DEFAULT_COALESCE_SECS);
        if log.is_empty() {
            // 空日志也导出：用户点了就该拿到文件（「本次没有编辑」本身是结论）。
            self.notice = Some("本次会话没有编辑记录，已导出空表".to_string());
        } else {
            self.notice = Some(format!("已导出编辑日志（{} 条记录）", log.len()));
        }
        let sheet = render_audit(log.records(), format);
        let stem = self
            .state
            .selected_file
            .as_ref()
            .and_then(|p| p.file_stem())
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "subtitle".to_string());
        let file_name = format!("{stem}-编辑日志.{}", format.extension());
        self.export_text_content(cx, file_name, sheet, format.label());
    }

    /// 导出**诊断报告**：把「程序实际解析到的路径、每个组件的状态、运行库能否解析」
    /// 打成一份可复制的纯文本。
    ///
    /// # 为什么需要它
    ///
    /// 「说 ffmpeg 缺失，可我明明有 ffmpeg」这类问题此前只能靠零散的日志行来回猜：
    /// 用户看不到程序把项目根锚到了哪、每个可执行文件实际是哪一个、它的依赖 DLL 是否
    /// 能解析。报告把这些一次问清楚，用户直接复制粘贴就能求助，把几天来回缩短到一条消息。
    ///
    /// 与质检表共用 `export_text_content`（同一套「另存为 + 后台写盘 + 选中文件」），
    /// 不另造导出链路。
    pub(crate) fn export_diagnostics(&mut self, cx: &mut Context<Self>) {
        let root = crate::utils::AppConfig::app_root_dir();
        let report = crate::utils::diagnostics::build_report(&self.state.config, &root);
        // 文件名带日期：用户可能对比「改配置前后」两份报告，带日期才分得清。
        let name = format!(
            "voice2word-diagnostics-{}.txt",
            chrono::Local::now().format("%Y%m%d-%H%M")
        );
        self.notice = Some("已导出诊断报告，可直接复制粘贴用于排查".to_string());
        self.export_text_content(cx, name, report, "诊断报告");
    }

    /// 把一段纯文本内容交给「另存为」对话框写出（质检表这类非字幕产物的共用出口）。    /// 把一段纯文本内容交给「另存为」对话框写出（质检表这类非字幕产物的共用出口）。
    ///
    /// 单独抽出来而不是复用 `export_with_save_dialog`：后者绑定的是「字幕片段 +
    /// SubtitleWriter」这条链路，而质检表是**已经渲染好的字符串**，再套一层写盘
    /// 适配器只会把两件不相干的事绕在一起。
    fn export_text_content(
        &mut self,
        cx: &mut Context<Self>,
        default_name: String,
        content: String,
        filter_label: &str,
    ) {
        let filter = filter_label.to_string();
        cx.spawn(async move |this, cx| {
            let ext = default_name.rsplit('.').next().unwrap_or("txt").to_string();
            let handle = rfd::AsyncFileDialog::new()
                .set_title("导出质检表")
                .set_file_name(&default_name)
                .add_filter(&filter, &[ext.as_str()])
                .save_file()
                .await;
            let Some(handle) = handle else { return };
            let path = handle.path().to_path_buf();
            // 写盘本身是毫秒级小文件，但保持与其它导出一致的「后台执行」习惯，
            // 免得将来内容变大时又要改一遍。路径 clone 一份进后台闭包：`path`
            // 在收尾的成功分支里还要用（打开资源管理器选中它）。
            let path_for_write = path.clone();
            let result = cx
                .background_executor()
                .spawn(async move { std::fs::write(&path_for_write, content.as_bytes()) })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(()) => {
                        // 打开所在目录并选中文件：用户下一步多半就是把它发出去。
                        let _ = std::process::Command::new("explorer")
                            .arg(format!("/select,{}", path.display()))
                            .spawn();
                    }
                    Err(e) => {
                        this.state.status = ProcessStatus::Failed(format!("质检表写出失败：{e}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(crate) fn open_models_dir(&mut self, cx: &mut Context<Self>) {
        let dir = crate::utils::AppConfig::resolve_path("models");
        // 全新解压的瘦包里 `models/` 可能还没建：先建出来，否则资源管理器会报
        // 「找不到路径」——那看起来像程序出错了，而用户只是还没下过东西。
        let dir = if dir.is_dir() {
            dir
        } else {
            let _ = std::fs::create_dir_all(&dir);
            dir
        };
        let _ = std::process::Command::new("explorer").arg(&dir).spawn();
        cx.notify();
    }

    /// 在资源管理器里定位某个可下载组件的文件（找不到就退到它所在目录）。
    ///
    /// 用 `explorer /select,<path>`：Windows 的资源管理器会打开父目录并**选中**该
    /// 文件，比只开目录再让用户自己找友好得多。文件不存在时 `/select` 会静默失败，
    /// 因此先判断存在性，不存在就退成打开目录。
    pub(crate) fn reveal_model_file(&mut self, item_id: &str, cx: &mut Context<Self>) {
        let Some(item) = crate::utils::model_download::item_by_id(item_id) else {
            self.state.download_status_msg = format!("未知组件：{item_id}");
            cx.notify();
            return;
        };
        let path = crate::utils::model_download::item_effective_path(item, &self.state.config);
        let (dir, select) = if path.is_file() {
            (path.parent().map(|p| p.to_path_buf()), Some(path))
        } else {
            (Some(crate::utils::AppConfig::resolve_path(".")), None)
        };
        let Some(dir) = dir else { return };
        let _ = std::fs::create_dir_all(&dir);
        let mut cmd = std::process::Command::new("explorer");
        match select {
            Some(p) => cmd.arg(format!("/select,{}", p.display())),
            None => cmd.arg(&dir),
        };
        let _ = cmd.spawn();
        cx.notify();
    }

    /// 打开系统原生文件对话框选择**本地大模型**（离线 Qwen 用的 GGUF 文件）。
    ///
    /// 与 [`MainWindow::choose_file`] 同一套写法（`cx.spawn` + `rfd::AsyncFileDialog`）：
    /// 同步的 `FileDialog` 会起 Win32 模态循环，而 GPUI 在渲染回调里正持有 RefCell
    /// 借用，模态循环一旦重入就是借用冲突崩溃。异步版把选择过程交给系统，UI 线程
    /// 不被阻塞，选完再回主线程写配置。
    ///
    /// 为什么单独加这个入口：离线模型路径此前只能手改 `config.toml`（用户实测反馈），
    /// 而「我自己下了个 GGUF 想用」是最自然的诉求——让他手抄一条绝对路径既不友好、
    /// 也容易抄错。过滤器给出 `.gguf`，但**不锁死**：llama.cpp 只吃 GGUF，
    /// 让用户先看到文件、再由校验给出可操作提示，比直接藏掉非 gguf 文件更好排错。
    pub(crate) fn choose_llm_model_file(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let handle = rfd::AsyncFileDialog::new()
                .set_title("选择本地大模型（GGUF）")
                .add_filter("GGUF 模型", &["gguf"])
                .add_filter("所有文件", &["*"])
                .pick_file()
                .await;
            if let Some(file_handle) = handle {
                let path = file_handle.path().to_path_buf();
                let _ = this.update(cx, |this, cx| {
                    this.set_local_model_path(path.to_string_lossy().to_string(), cx);
                });
            }
        })
        .detach();
    }

    /// 接纳一个刚导入的媒体文件：立刻发布选中状态，再在后台探测时长。
    ///
    /// 「浏览本地文件」与「拖拽文件进窗口」共用这一条路径，避免两种入口
    /// 各写一套时长探测逻辑而产生行为漂移。
    pub(crate) fn adopt_media_file(&mut self, file: PathBuf, cx: &mut Context<Self>) {
        if !file.is_file() {
            self.state.status =
                ProcessStatus::Failed(format!("无法导入：{} 不是有效文件", file.display()));
            cx.notify();
            return;
        }
        info!("用户导入了转写文件: {:?}", file);
        let ffmpeg_path = crate::utils::AppConfig::resolve_path(&self.state.config.paths.ffmpeg);
        // 先把选中状态发布出去，让界面立刻有反馈；ffmpeg 探测放到后台执行器，
        // 避免阻塞这次实体更新闭包。
        self.state.transcribe_duration = 0.0;
        self.state.transcribe_file = Some(file.clone());
        // 换了文件 → 「0 秒命中」的缓存结果必然作废。不清理的话，
        // 新文件会沿用上一个文件的缓存命中信息（界面显示错误的句数）。
        self.state.invalidate_cached_transcription();
        self.state.status = ProcessStatus::Idle;
        self.state.active_tab = WorkspaceTab::Generate;
        cx.notify();

        let duration_file = file.clone();
        cx.spawn(async move |this, cx| {
            let ffmpeg = crate::engines::FFmpegEngine::new(ffmpeg_path);
            let duration = cx
                .background_executor()
                .spawn(async move { ffmpeg.get_duration(&duration_file) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.state.transcribe_file.as_ref() == Some(&file) {
                    this.state.transcribe_duration = duration;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// 导入一个**已有的字幕文件**（SRT / VTT / TXT）作为当前工程，跳过转写。
    ///
    /// # 为什么需要它
    ///
    /// 项目能写出八种字幕格式却一种都读不回来：手上有一份同事给的 SRT、或想接着改
    /// 上次的导出，此前只能把视频重新转写一遍。这个入口补上缺失的导入链路。
    ///
    /// # 几个刻意的取舍
    ///
    /// - **不自动匹配视频**：导入的字幕是否对得上某个视频，程序无从判断（时间轴可能
    ///   被剪过）。直接发布成当前工程，用户自己决定要不要再选视频。
    /// - **清空撤销栈**：与「转写完成」同一处理——上一支片子的快照留在栈里，Ctrl+Z
    ///   会把旧字幕灌回来并按新工程落库，属于数据损坏。
    /// - **TXT 的时间是编出来的**：`reader` 按字数合成占位时间轴，这里必须明说，
    ///   否则用户导出 SRT 时会以为时间是准的。
    /// - **不落库**：导入的工程没有对应的媒体记录，`active_task_id` 置 `None`，
    ///   用户要留存就自己导出（或后续接一个「另存为工程」）。
    pub(crate) fn import_subtitle_file(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let handle = rfd::AsyncFileDialog::new()
                .set_title("导入字幕文件")
                .add_filter("字幕文件", &["srt", "vtt", "txt"])
                .pick_file()
                .await;
            let Some(handle) = handle else { return };
            let path = handle.path().to_path_buf();
            let result = cx
                .background_executor()
                .spawn(async move {
                    let segs = crate::subtitle::reader::read_subtitle_file(&path)?;
                    Ok::<_, anyhow::Error>((path, segs))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok((path, mut segs)) => {
                        if segs.is_empty() {
                            this.state.status =
                                ProcessStatus::Failed("字幕文件里没有可用的字幕行".to_string());
                            cx.notify();
                            return;
                        }
                        // 句序号重新按输出顺序编号：外部文件的序号常常是错的或重复的，
                        // 而下游（选中、质检、落库）全部按 `index` 索引。
                        for (i, seg) in segs.iter_mut().enumerate() {
                            seg.index = i + 1;
                        }
                        let count = segs.len();
                        let is_txt = path
                            .extension()
                            .and_then(|e| e.to_str())
                            .map(|e| e.eq_ignore_ascii_case("txt"))
                            .unwrap_or(false);
                        let stem = path
                            .file_stem()
                            .map(|s| s.to_string_lossy().to_string())
                            .unwrap_or_else(|| "导入的字幕".to_string());

                        this.state.segments = segs;
                        // 导入的字幕不属于历史库里的任何一条记录：置空避免把编辑
                        // 写回到别的工程的库行上（那是静默数据损坏）。
                        this.state.active_task_id = None;
                        this.state.selected_file = Some(path.clone());
                        this.state.reset_edit_history();
                        this.state.bump_segments_revision();
                        this.state.clear_streaming();
                        this.state.status = ProcessStatus::Idle;
                        if let Some(first) = this.state.segments.first() {
                            this.state.select_segment(first.index);
                        }
                        // 导入的是别人的字幕：波形/首帧都对不上，先清掉旧工程的缓存，
                        // 免得监视器还显示上一支片子的画面。
                        this.state.preview_source = None;
                        this.state.waveform = None;
                        this.state.active_tab = WorkspaceTab::Editor;
                        this.subtitle_list_followed_sel = None;

                        self_notice_import(this, &stem, count, is_txt);
                        cx.notify();
                    }
                    Err(e) => {
                        this.state.status = ProcessStatus::Failed(format!("导入字幕失败：{e}"));
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    /// 处理从操作系统拖入窗口的文件（GPUI 把外部文件拖放包装成一次内部 drag/drop）。
    ///
    /// 拖入多个文件时全部进入批量队列 (F-012)：先入队并统一探测时长，再等用户
    /// 点「开始全部」。单个文件仍走原有的「选中 + 探测时长」路径，保持单文件
    /// 交互手感不变。全部扩展名都不支持时给出明确提示，而不是静默无反应。
    pub(crate) fn handle_dropped_files(&mut self, paths: &[PathBuf], cx: &mut Context<Self>) {
        // 拖进来的可能是**目录**（把一门课的文件夹整个拖进来是最自然的动作）。
        // 此前只按扩展名筛文件、目录被静默丢弃，用户拖了个文件夹进去看到的是
        // 「不是支持的音视频格式」——既没告诉他目录其实支持，也没说清一个文件都
        // 没找到。现在统一交给 `media_scan` 展开：目录递归、文件直收、去重、
        // 跳过原因结构化返回，好让这里的提示是有信息量的。
        let outcome = crate::utils::media_scan::collect_media_files(
            paths,
            &MEDIA_EXTS,
            crate::utils::media_scan::DEFAULT_MAX_DEPTH,
        );
        // 只借用 `files`，不把它 move 出去——后面 `outcome` 还要用来算提示摘要
        // （收了几个、跳过什么）。`files.len()` 先存下来，避免与借用打架。
        let media = &outcome.files;
        let found = media.len();

        match found {
            0 => {
                self.state.status = ProcessStatus::Failed(self.drop_failure_message(&outcome));
                cx.notify();
            }
            1 => {
                // 单个文件仍走「选中 + 探测时长」的原路径，保持单文件手感不变。
                // 但从目录里扫出来的这一个文件**不该**变成「当前文件」——用户拖的是
                // 一个文件夹，期待的是队列，不是把里面第一个文件当成当前工程。
                if outcome.dirs_scanned > 0 || paths.iter().any(|p| p.is_dir()) {
                    let added = self.state.enqueue_files(media.clone());
                    self.state.active_tab = WorkspaceTab::Generate;
                    self.probe_queue_durations(cx);
                    self.notice = Some(format!("已从文件夹加入 {added} 个文件到批量队列"));
                    cx.notify();
                } else {
                    self.adopt_media_file(media[0].clone(), cx)
                }
            }
            _ => {
                let added = self.state.enqueue_files(media.clone());
                if added == 0 {
                    self.state.status =
                        ProcessStatus::Failed("这些文件已经在批量队列里，未重复添加".to_string());
                    cx.notify();
                    return;
                }
                info!("批量队列新增 {} 个文件", added);
                // 队列面板在转写页，切过去用户才看得见刚入队的东西
                self.state.active_tab = WorkspaceTab::Generate;
                self.probe_queue_durations(cx);
                // 拖了目录、里面有一堆非媒体文件是常态：必须交代「收了多少、跳过了
                // 什么」，否则用户会以为漏扫了。
                self.notice = Some(Self::ingest_summary(added, &outcome));
                cx.notify();
            }
        }
    }

    /// 一个文件都没收到时，把「为什么」拼成可操作的中文提示。
    ///
    /// 分三类说：① 完全没有可识别文件（附支持的扩展名）；② 有跳过项，逐类汇总次数
    /// 并给出**第一条**具体路径（全列出来会把状态栏挤爆，而用户通常只需要一个样例
    /// 就能定位问题）；③ 目录太深——这是唯一「改个设置就能救」的情形，单独点出来。
    fn drop_failure_message(&self, outcome: &crate::utils::media_scan::ScanOutcome) -> String {
        const SUPPORTED: &str = "mp4/mkv/mov/avi/flv/webm/mp3/wav/flac/m4a";
        if outcome.skipped.is_empty() {
            return format!(
                "没有找到可处理的音视频文件（支持 {SUPPORTED}）。\
                 若拖入的是文件夹，请确认里面有这些格式的文件。"
            );
        }
        // 按原因归类计数，并把每类的首个样例路径带上
        let mut by_reason: Vec<(crate::utils::media_scan::SkipReason, usize, PathBuf)> = Vec::new();
        for (path, reason) in &outcome.skipped {
            match by_reason.iter_mut().find(|(r, _, _)| r == reason) {
                Some((_, n, _)) => *n += 1,
                // SkipReason 未派生 Copy：clone 一份存进汇总（四个无载荷变体，
                // 克隆成本为零）。
                None => by_reason.push((reason.clone(), 1, path.clone())),
            }
        }
        let detail = by_reason
            .iter()
            .map(|(reason, n, sample)| {
                // `SkipReason::message` 取 `self`（四个无载荷变体，clone 零成本）；
                // 这里拿到的是引用，clone 一份再取文案。
                format!(
                    "{}（{n} 项，如 {}）",
                    reason.clone().message(),
                    sample.display()
                )
            })
            .collect::<Vec<_>>()
            .join("；");
        format!("没有扫描到可处理的音视频文件：{detail}。支持 {SUPPORTED}。")
    }

    /// 成功导入多个文件时的一行摘要（收了几个、跳过了什么）。
    fn ingest_summary(added: usize, outcome: &crate::utils::media_scan::ScanOutcome) -> String {
        let dirs = if outcome.dirs_scanned > 0 {
            format!("（扫描了 {} 个文件夹）", outcome.dirs_scanned)
        } else {
            String::new()
        };
        if outcome.skipped.is_empty() {
            return format!("已加入 {added} 个文件到批量队列{dirs}");
        }
        // 只提「值得用户动手」的两类：太深（换个更具体的目录再拖）与不可读
        // （盘没挂 / 没权限）。**按枚举匹配，不按文案子串匹配**——后者会在
        // 文案改一个字之后静默失效（提示从此永远为空，且没有任何报错），
        // 这正是这一版要避免的那类「悄悄坏掉」。
        //
        // 「非媒体扩展名」不在这里报：拖一个课程文件夹进来，里面混着 .txt/.pdf
        // 是常态，逐条报只会把真正要紧的提示淹掉（失败分支才需要展开讲）。
        use crate::utils::media_scan::SkipReason;
        // 顺序即提示里出现的顺序：先讲「扫不动」的（太深），再讲「根本没看」的。
        let mut notable: Vec<String> = Vec::new();
        for reason in [
            SkipReason::TooDeep,
            SkipReason::Unreadable,
            SkipReason::Symlink,
        ] {
            let n = outcome.skipped.iter().filter(|(_, r)| *r == reason).count();
            if n > 0 {
                let what = match reason {
                    SkipReason::TooDeep => "个文件夹层级过深未扫",
                    SkipReason::Unreadable => "个路径无法读取",
                    SkipReason::Symlink => "个快捷方式未跟随",
                    // 上面这个列表不含该类；真加了新变体进来会在编译期报错，
                    // 强迫作者顺手补一条文案——比运行时兜底更可靠。
                    SkipReason::UnsupportedExtension => "个非媒体文件已跳过",
                };
                notable.push(format!("{n} {what}"));
            }
        }
        if notable.is_empty() {
            format!("已加入 {added} 个文件到批量队列{dirs}")
        } else {
            format!(
                "已加入 {added} 个文件到批量队列{dirs}；{}",
                notable.join("、")
            )
        }
    }

    /// 批量选择文件加入队列（转写页「批量导入」按钮）
    pub(crate) fn choose_batch_files(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let handles = rfd::AsyncFileDialog::new()
                .add_filter("音视频文件", &MEDIA_EXTS)
                .pick_files()
                .await;
            let Some(handles) = handles else { return };
            let files: Vec<PathBuf> = handles.iter().map(|h| h.path().to_path_buf()).collect();
            let _ = this.update(cx, |this, cx| {
                this.ingest_paths(files, cx);
            });
        })
        .detach();
    }

    /// 选择一个**文件夹**，把里面（含子目录）的媒体文件全部加入队列。
    ///
    /// 与「批量导入文件」分开两个按钮，是因为系统文件对话框一次只能是「选文件」或
    /// 「选文件夹」二选一，混在一起会让用户点开发现选不了目录而困惑。拖拽那条路
    /// 则两者都收（见 `handle_dropped_files`）。
    pub(crate) fn choose_batch_folder(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let handle = rfd::AsyncFileDialog::new()
                .set_title("选择包含音视频的文件夹")
                .pick_folder()
                .await;
            let Some(handle) = handle else { return };
            let dir = handle.path().to_path_buf();
            let _ = this.update(cx, |this, cx| {
                this.ingest_paths(vec![dir], cx);
            });
        })
        .detach();
    }

    /// 把一批「用户给的路径」（文件 / 目录混合）展开后入队，并给出有信息量的反馈。
    ///
    /// 「批量导入文件」「选择文件夹」两条入口共用它，因此「收了多少、跳过什么、
    /// 一个都没收到时为什么」这套提示只有一份实现，不会两处漂移。
    pub(crate) fn ingest_paths(&mut self, inputs: Vec<PathBuf>, cx: &mut Context<Self>) {
        let outcome = crate::utils::media_scan::collect_media_files(
            &inputs,
            &MEDIA_EXTS,
            crate::utils::media_scan::DEFAULT_MAX_DEPTH,
        );
        if outcome.files.is_empty() {
            self.state.status = ProcessStatus::Failed(self.drop_failure_message(&outcome));
            cx.notify();
            return;
        }
        let found = outcome.files.len();
        // clone 而不是 move：`outcome` 后面还要用来生成摘要（收了多少、跳过什么）。
        let added = self.state.enqueue_files(outcome.files.clone());
        if added == 0 {
            self.state.status =
                ProcessStatus::Failed(format!("这 {found} 个文件已经在批量队列里，未重复添加"));
        } else {
            info!("批量队列新增 {} 个文件", added);
            self.notice = Some(Self::ingest_summary(added, &outcome));
        }
        self.state.active_tab = WorkspaceTab::Generate;
        self.probe_queue_durations(cx);
        cx.notify();
    }

    /// 为队列中尚未探测时长的条目补齐时长（批量完成弹窗要按累计时长做统计）。
    ///
    /// 串行探测而不是并发：`ffmpeg -i` 对本地文件是毫秒级操作，并发派发只会让
    /// 进程表瞬间多出一堆短命进程，收益为零。
    pub(crate) fn probe_queue_durations(&mut self, cx: &mut Context<Self>) {
        let pending: Vec<PathBuf> = self
            .state
            .batch_queue
            .iter()
            .filter(|item| item.duration <= 0.0)
            .map(|item| item.path.clone())
            .collect();
        if pending.is_empty() {
            return;
        }
        let ffmpeg_path = crate::utils::AppConfig::resolve_path(&self.state.config.paths.ffmpeg);
        cx.spawn(async move |this, cx| {
            for path in pending {
                let ffmpeg = crate::engines::FFmpegEngine::new(ffmpeg_path.clone());
                let probe_target = path.clone();
                let duration = cx
                    .background_executor()
                    .spawn(async move { ffmpeg.get_duration(&probe_target) })
                    .await;
                let probe_key = path.clone();
                let _ = this.update(cx, |this, cx| {
                    // 用路径而不是下标回写：探测期间用户可能删过队列条目，
                    // 下标会整体前移，按下标写会张冠李戴。
                    if let Some(idx) = this.state.queue_index_of(&probe_key) {
                        this.state.batch_queue[idx].duration = duration;
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// 从队列里移除一条（用户手动剔除不想要的片源）
    pub(crate) fn remove_queue_item(&mut self, idx: usize, cx: &mut Context<Self>) {
        if self.state.remove_queue_item(idx) {
            cx.notify();
        }
    }

    /// 清空队列并退出批量模式（已转写完成、已落库的工程不受影响）
    pub(crate) fn clear_batch_queue(&mut self, cx: &mut Context<Self>) {
        if matches!(self.state.status, ProcessStatus::Processing { .. }) {
            self.request_cancel_processing(cx);
        }
        self.state.clear_batch_queue();
        cx.notify();
    }

    /// 删除一条视频库记录，并把「当前工程被删掉了」这件事显式告诉用户。
    ///
    /// `AppState::delete_task_record` 在删掉的恰好是当前打开的工程时，会**静默**把
    /// 工作区换成列表里的下一条记录。用户回到剪辑台会发现字幕变成了另一个视频的，
    /// 却没有任何线索说明发生了什么。这里在删除后检查当前工程是否真的被换掉，
    /// 是则发一条**中性提示**（不是错误——删除本身成功了）说明切换到了哪一条。
    pub(crate) fn delete_task_record(&mut self, id: i64, cx: &mut Context<Self>) {
        // 连带清掉这个任务在界面侧的缓存。这两个 map 按 task_id 累积，
        // 而 `recent_tasks` 只保留最近 50 条——不清理的话，用户长时间使用
        // （不断导入/删除）后缓存会一直涨，且全是查不到的僵尸条目。
        // 注意必须放在删除**之前**：删除后 `recent_tasks` 里已找不到该 id，
        // 也就无从得知要清哪一条。
        self.library_thumbs.remove(&id);
        self.library_sizes.remove(&id);
        let was_current = self
            .state
            .recent_tasks
            .iter()
            .find(|t| t.id == id)
            .map(|t| {
                self.state.selected_file.as_ref().is_some_and(|p| {
                    p.to_string_lossy().replace('\\', "/") == t.file_path.replace('\\', "/")
                })
            })
            .unwrap_or(false);

        self.state.delete_task_record(id);

        self.notice = Some(if was_current {
            match self
                .state
                .selected_file
                .as_ref()
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
            {
                Some(name) => format!("已删除当前工程，工作区已切换到「{name}」"),
                None => "已删除当前工程，工作区已清空".to_string(),
            }
        } else {
            "已删除该记录".to_string()
        });

        if self.state.selected_file.is_some() {
            self.trigger_extract_frame(cx);
        }
        cx.notify();
    }

    /// 「开始全部」：从第一个待处理项起，连续转写直到队列跑完。
    ///
    /// 失败项会被重新排队——用户点「开始全部」的语义就是「把没成功的再跑一遍」，
    /// 已经成功的条目则跳过，不重复烧算力。
    pub(crate) fn start_batch_queue(&mut self, cx: &mut Context<Self>) {
        if matches!(self.state.status, ProcessStatus::Processing { .. }) {
            return;
        }
        if self.state.batch_queue.is_empty() {
            self.state.status = ProcessStatus::Failed("批量队列是空的，先导入文件".to_string());
            cx.notify();
            return;
        }
        for item in self.state.batch_queue.iter_mut() {
            if item.state.is_failed() {
                item.state = QueueState::Pending;
            }
        }
        let Some(next) = self.state.queue_next_actionable() else {
            self.state.status =
                ProcessStatus::Failed("队列中的文件都已转写完成，无需重复处理".to_string());
            cx.notify();
            return;
        };
        self.state.batch_running = true;
        let path = self.state.batch_queue[next].path.clone();
        self.start_processing_for(path, cx);
    }

    /// 终止批量队列：停掉当前任务并停止续跑（已完成的条目保留结果）
    pub(crate) fn cancel_batch_queue(&mut self, cx: &mut Context<Self>) {
        self.state.batch_running = false;
        if matches!(self.state.status, ProcessStatus::Processing { .. }) {
            self.request_cancel_processing(cx);
        } else {
            cx.notify();
        }
    }

    /// 取出队列里下一个待处理项并立即开跑；返回 `true` 表示已续上。
    ///
    /// 队列跑完时在这里收尾：复位批量模式并弹出批量汇总弹窗。
    fn advance_batch_queue(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.state.batch_running {
            return false;
        }
        match self.state.queue_next_actionable() {
            Some(next) => {
                let path = self.state.batch_queue[next].path.clone();
                self.start_processing_for(path, cx);
                true
            }
            None => {
                self.finish_batch_queue(cx);
                false
            }
        }
    }

    /// 批量队列收尾：统计结果并弹出汇总弹窗。
    ///
    /// 队列跑完后工作台停在上一条完成的工程上（`Finished` 分支已同步过），
    /// 这里只负责把「这一批到底成了几个、败了几个」讲清楚。
    fn finish_batch_queue(&mut self, cx: &mut Context<Self>) {
        self.state.batch_running = false;
        self.state.batch_active = None;
        let (done, failed, segments) = self.state.queue_summary();
        let total = self.state.batch_queue.len();
        let total_duration = self.state.queue_total_duration();
        let failed_names = self.state.queue_failed_names();
        info!("批量转写结束：{done} 成功 / {failed} 失败 / 共 {total} 个文件");
        self.state.status = ProcessStatus::Idle;
        // 工作台停在最后一条完成的工程上，波形在整个队列跑完后再抽一次即可
        self.ensure_waveform(cx);
        self.completion_dialog = Some(CompletionDialogInfo {
            file_name: format!("本次共处理 {total} 个文件"),
            segment_count: segments,
            total_duration,
            metrics: None,
            batch: Some(BatchSummary {
                total,
                done,
                failed,
                segments,
                total_duration,
                failed_names,
            }),
        });
        cx.notify();
    }

    /// 智能缓存命中处理：直接 0 秒载入已缓存的解析结果
    ///
    /// 入参是 `Arc<TaskRecord>`：调用点在渲染路径上拿到的是缓存的共享引用，
    /// 这里按需 `(*cached).clone()` 展开一次即可——载入是**一次性**动作，
    /// 不像渲染那样每帧发生，复制一次整份字幕可接受。
    pub(crate) fn load_cached_result(
        &mut self,
        cx: &mut Context<Self>,
        cached: std::sync::Arc<crate::storage::TaskRecord>,
    ) {
        let filename = cached.file_name.clone();
        let seg_count = cached.segments.len();
        let total_dur = cached.duration;
        let metrics = cached.metrics.clone();

        info!("智能缓存命中: 0 秒载入 {:?}", filename);
        self.state.load_from_cache((*cached).clone());
        self.state.active_tab = crate::app::state::WorkspaceTab::Editor;
        self.ensure_waveform(cx);

        self.completion_dialog = Some(CompletionDialogInfo {
            file_name: filename,
            segment_count: seg_count,
            total_duration: total_dur,
            metrics,
            batch: None,
        });
        cx.notify();
    }

    /// 触发流水线处理（单文件入口：跑完弹单文件完成框）
    pub(crate) fn start_processing(&mut self, cx: &mut Context<Self>) {
        let Some(file) = self.state.transcribe_file.clone() else {
            return;
        };
        // 单文件入口显式退出批量续跑语义：否则这一条跑完会把整个队列也顺手带跑，
        // 用户点「开始转写」时并没有这个预期。
        self.state.batch_running = false;
        self.state.batch_active = None;
        self.start_processing_for(file, cx);
    }

    /// 当前选中的识别档位缺什么就返回一句可操作的中文原因；齐备则 `None`。
    ///
    /// 抽成函数是为了让「开工守卫」与将来的其它入口（例如快捷键、命令面板）
    /// 复用同一份判定，而不是各自拼字符串——那类复制粘贴正是「文案一处改、另一处
    /// 忘了改」的来源。
    pub(crate) fn missing_engine_reason(&self) -> Option<String> {
        use crate::app::WhisperModelTier;
        let tier = self.state.whisper_model_tier;
        if tier == WhisperModelTier::SenseVoice {
            if !self.state.model_is_present("sensevoice-model") {
                return Some(
                    "SenseVoice 模型未就位：请在「性能设置 → 模型与组件」下载，或改用 Whisper 档位"
                        .to_string(),
                );
            }
            return None;
        }
        // Whisper 各档：判据取**管线真正会加载的那个文件**（`model_relative_path`
        // 已含「首选量化缺失时平滑回退」的逻辑），而不是 `model_is_present`。
        // 两者在正常情形下同结论，但用户把 `paths.whisper_model` 指到一个**非档位
        // 命名**的自备模型时，`model_is_present` 会因「名字对不上任何档位」判为缺失，
        // 而管线其实跑得起来——用实际路径判定就不会误拦。
        let will_load = crate::utils::AppConfig::resolve_path(&tier.model_relative_path());
        (!will_load.is_file()).then(|| {
            format!(
                "{} 模型未就位：请在「性能设置 → Whisper 模型档位」点「下载」，或改选已就位的档位",
                tier.label()
            )
        })
    }

    /// 启动一次转写，显式指定输入文件。
    ///
    /// 单文件入口与批量续跑共用这一条启动路径：差别只在「跑完之后做什么」，
    /// 启动前的准备（停预览、拼运行时参数、派发后台线程）完全一致。
    pub(crate) fn start_processing_for(&mut self, file: PathBuf, cx: &mut Context<Self>) {
        if matches!(self.state.status, ProcessStatus::Processing { .. }) {
            return;
        }
        // 开工前先确认**选中的档位**确实有模型。
        //
        // 档位下拉现在会明示「已就位 / 未下载」并就地给下载按钮，用户完全可以先选中
        // 一档再决定要不要下。若这里不拦，`model_override` 会指向一个不存在的文件，
        // 报错来自 whisper-cli 的 stderr（「failed to load model」之类），
        // 用户看不出是「档位没下」还是「文件坏了」。这里提前给可操作的中文提示，
        // 并把他送回能下载的那个界面。
        //
        // 只在**单文件入口**拦：批量续跑时若中途某档位文件被删，报错也应落到那一条
        // 任务上，而不是整批静默停住——不过批量与单文件共用本函数，所以这条守卫对
        // 两者一致：都停在开工前，都不会把任务跑成半截。
        if let Some(reason) = self.missing_engine_reason() {
            self.state.status = ProcessStatus::Failed(reason);
            cx.notify();
            return;
        }
        // Preview decoding is CPU-heavy on a CPU-only machine. Stop it before
        // Whisper starts so the ASR workers get the available cores and memory
        // bandwidth instead of competing with 25 fps BGRA conversion.
        if self.state.is_playing {
            self.state.current_time = self.state.video_player.current_play_time();
        }
        self.state.video_player.stop();
        self.state.is_playing = false;
        self.play_tick_generation = self.state.video_player.generation();

        // 批量模式：把这一条标记为「转写中」，并把入队时探测好的时长喂给进度条。
        // 时长必须在这里回填——管线只上报进度，不上报总时长。
        if self.state.batch_running {
            if let Some(idx) = self.state.queue_index_of(&file) {
                self.state.batch_active = Some(idx);
                self.state.mark_queue_running(idx);
                let probed = self.state.batch_queue[idx].duration;
                if probed > 0.0 {
                    self.state.transcribe_duration = probed;
                }
            }
        }

        let input_file = file;
        self.state.transcribe_file = Some(input_file.clone());

        let (tx, mut rx) = mpsc::unbounded_channel();
        self.state.clear_streaming();
        self.state.cancel_requested = false;
        // 尽早在 UI 线程作废上一轮的取消状态（管线侧 + 引擎侧）。**必须留在 UI 线程、
        // 且早于下面 spawn 任务线程**：把它挪进任务线程就是原本那条「终止转写失效」
        // 的竞态——线程刚起来、还没跑到复位，用户已点「终止」置了位，随后被任务自己
        // 清零，转写照跑完。放在这里之后，任务线程里不再有任何清标志的写方，派发口
        // 又由函数开头的 `status != Processing` 守卫兜底，上一轮的取消不会误伤新任务。
        self.state.pipeline.begin_task();
        self.state.status = ProcessStatus::Processing {
            stage: "准备中...".to_string(),
            progress: 0.0,
            detail: "正在启动异步任务管线".to_string(),
        };
        cx.notify();

        let pipeline = self.state.pipeline.clone();
        let lang = if self.state.language == "auto" {
            None
        } else {
            Some(self.state.language.clone())
        };
        let fmt = self.state.output_format.clone();
        let polish = self.state.enable_polish;
        let polish_mode = Some(self.state.polish_mode.as_str().to_string());
        let threads = Some(self.state.whisper_threads);

        // 根据档位计算运行时模型路径覆盖（绝对路径）
        let model_override = {
            let rel = self.state.whisper_model_tier.model_relative_path();
            let abs = crate::utils::AppConfig::resolve_path(&rel);
            Some(abs)
        };
        let rescue_logprob = self.state.config.pipeline.whisper_rescue_logprob;
        // 音频前端预处理：把配置里的开关映射成管线参数。默认全开——
        // 它改的是「喂给模型的音频量」，是唯一还有量级空间的提速方向。
        let preprocess = crate::core::pipeline::PreprocessOptions {
            enabled: self.state.config.pipeline.preprocess_enabled,
            filters: crate::engines::SpeechFilterOptions {
                highpass_hz: self.state.config.pipeline.preprocess_highpass_hz,
                denoise: self.state.config.pipeline.preprocess_denoise,
                normalize: self.state.config.pipeline.preprocess_normalize,
                target_rate: crate::engines::ASR_SAMPLE_RATE,
            },
            compaction: crate::engines::CompactionConfig::default(),
            compact: self.state.config.pipeline.preprocess_compact,
            min_saving: self.state.config.pipeline.preprocess_min_saving,
        };
        let whisper_options = crate::core::pipeline::WhisperRuntimeOptions {
            audio_speed: self.state.config.pipeline.whisper_audio_speed,
            vad_threshold: self.state.config.pipeline.whisper_vad_threshold,
            preprocess,
        };
        // 说话人分离（F-015）：开关与期望人数都取自配置，关时管线完全跳过该阶段
        let diarization = crate::core::pipeline::DiarizationOptions {
            enabled: self.state.config.pipeline.enable_diarization,
            speakers: self.state.config.pipeline.speaker_count,
        };

        // 后台通过独立线程执行 Tokio 异步管线，不阻塞 UI 主线程
        let in_file = input_file.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                if let Err(err) = pipeline
                    .run_with_options(
                        in_file,
                        None,
                        lang,
                        fmt,
                        polish,
                        polish_mode,
                        threads,
                        model_override,
                        rescue_logprob,
                        whisper_options,
                        diarization,
                        tx.clone(),
                    )
                    .await
                {
                    let _ = tx.send(PipelineEvent::Error(format!("转写失败: {err}")));
                }
            });
        });

        // 使用 GPUI 官方实体协程封装，避免复用 AsyncApp 引用造成重入借用冲突。
        cx.spawn(async move |this, cx| {
            while let Some(first_event) = rx.recv().await {
                // Whisper 会连续产生大量事件，先聚合一个短窗口，保证 GPUI
                // 每帧只执行一次实体更新，避免高频重入 App RefCell。
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(80))
                    .await;
                let mut events = vec![first_event];
                while let Ok(event) = rx.try_recv() {
                    events.push(event);
                }

                let _ = this.update(cx, |this, cx| {
                    for event in events {
                        match event {
                            PipelineEvent::StageChanged(stage) => {
                                if let ProcessStatus::Processing {
                                    stage: ref mut s, ..
                                } = &mut this.state.status
                                {
                                    *s = stage;
                                }
                            }
                            PipelineEvent::Progress {
                                stage,
                                progress,
                                detail,
                            } => {
                                this.state.status = ProcessStatus::Processing {
                                    stage,
                                    progress,
                                    detail,
                                };
                            }
                            PipelineEvent::SegmentStream(seg) => {
                                this.state.push_stream_segment(seg);
                            }
                            PipelineEvent::Finished(segments, metrics) => {
                                this.state.clear_streaming();
                                if this.state.cancel_requested {
                                    // 用户主动终止：静默收尾，不弹完成框、不写历史库
                                    this.state.cancel_requested = false;
                                    this.state.status = ProcessStatus::Idle;
                                    this.state.transcribe_file = None;
                                    this.state.transcribe_duration = 0.0;
                                    // 批量模式下终止：当前项记为取消，并停掉续跑。
                                    // 已经跑完的条目保留结果，不清零。
                                    this.state
                                        .finish_active_queue_item(Err("已取消".to_string()));
                                    this.state.batch_running = false;
                                } else if segments.is_empty() {
                                    this.state.status = ProcessStatus::Failed(
                                        "未能识别出任何有效字幕，请检查音频音量或识别语言设置"
                                            .to_string(),
                                    );
                                    this.state.finish_active_queue_item(Err(
                                        "未识别出有效字幕".to_string()
                                    ));
                                } else {
                                    let finished_file = this.state.transcribe_file.take();
                                    let file_path =
                                        finished_file.or_else(|| this.state.selected_file.clone());
                                    let filename = file_path
                                        .as_ref()
                                        .and_then(|p| p.file_name())
                                        .and_then(|s| s.to_str())
                                        .unwrap_or("media.mp4")
                                        .to_string();
                                    let total_dur = if this.state.transcribe_duration > 0.0 {
                                        this.state.transcribe_duration
                                    } else if this.state.total_duration > 0.0 {
                                        this.state.total_duration
                                    } else {
                                        segments.last().map(|s| s.end).unwrap_or(0.0)
                                    };
                                    let seg_count = segments.len();
                                    // 用 batch_running 而不是 batch_active 判断：用户删掉队列里
                                    // 正在跑的条目时 batch_active 会被清空，但这一批仍在续跑。
                                    // 若按 batch_active 判断，就会在批量中途弹出单文件完成框
                                    // 并逐条重抽波形。
                                    let in_batch = this.state.batch_running;

                                    // 1. 存入数据库历史记录 (包含 5 阶段性能统计指标)
                                    let mut new_task_id: Option<i64> = None;
                                    if let Some(ref path) = file_path {
                                        new_task_id = this
                                            .state
                                            .db
                                            .insert_task(
                                                &path.to_string_lossy(),
                                                &filename,
                                                total_dur,
                                                "completed",
                                                &segments,
                                                Some(&metrics),
                                            )
                                            .ok();
                                        this.state.refresh_recent_tasks();
                                        // 刚写入了新记录 → 让「0 秒命中」缓存失效，
                                        // 否则再次选中同一文件会显示旧的命中状态
                                        this.state.invalidate_cached_transcription();
                                    }

                                    // 2. 将本次成果同步至剪辑校对工作台。
                                    // 覆盖前先把上一个工程的未保存改动落库：批量连跑时
                                    // 用户可能在上一支片子上改过字幕，不能默默丢掉。
                                    // 注意顺序：这一步必须用**旧**的 active_task_id 写回，
                                    // 所以新 id 要等 segments 换完之后才登记。
                                    this.state.flush_segments_if_dirty();
                                    if let Some(path) = file_path {
                                        this.state.selected_file = Some(path.clone());
                                        this.state.preview_source = Some(path);
                                    }
                                    this.state.segments = segments;
                                    this.state.active_task_id = new_task_id;
                                    // 新一轮转写的结果替换了整份文档：撤销栈里还是上一支
                                    // 片子的快照，留着的话 Ctrl+Z 会把上一支的字幕灌进这一支，
                                    // 并按新的 active_task_id 落库覆盖刚写好的记录。
                                    this.state.reset_edit_history();
                                    this.state.bump_segments_revision();
                                    if let Some(first) = this.state.segments.first() {
                                        this.state.select_segment(first.index);
                                    }
                                    this.state.total_duration = total_dur;

                                    // 3. 语音转写界面彻底解耦复位（等待下一次导入）
                                    this.state.status = ProcessStatus::Idle;
                                    this.state.transcribe_file = None;
                                    this.state.transcribe_duration = 0.0;

                                    if in_batch {
                                        // 批量模式：只结算这一条。波形提取与完成弹窗都留到
                                        // 队列跑完再一次性处理，否则每完成一条都要重抽一次
                                        // 波形，纯属白烧解码。
                                        this.state.finish_active_queue_item(Ok(seg_count));
                                    } else {
                                        this.ensure_waveform(cx);

                                        // 4. 弹出全屏转写完成提醒弹窗 (包含性能基准看板)
                                        this.completion_dialog = Some(CompletionDialogInfo {
                                            file_name: filename,
                                            segment_count: seg_count,
                                            total_duration: total_dur,
                                            metrics: Some(metrics),
                                            batch: None,
                                        });
                                    }
                                }
                                // 批量续跑：队列没跑完就立刻取下一条，跑完则弹批量汇总
                                if this.state.batch_running {
                                    this.advance_batch_queue(cx);
                                }
                            }
                            PipelineEvent::Error(err) => {
                                if this.state.cancel_requested {
                                    // 终止导致的子进程报错按取消处理，不显示为失败
                                    this.state.cancel_requested = false;
                                    this.state.status = ProcessStatus::Idle;
                                    this.state.transcribe_file = None;
                                    this.state.transcribe_duration = 0.0;
                                    this.state
                                        .finish_active_queue_item(Err("已取消".to_string()));
                                    this.state.batch_running = false;
                                } else if this.state.batch_running {
                                    // 批量模式：单个文件出错不该拖垮整批。失败原因记在队列卡片上，
                                    // 立刻续跑下一个，最后在汇总弹窗里一并点名。
                                    // 这里同样按 batch_running 判断，理由见 Finished 分支。
                                    this.state.transcribe_file = None;
                                    this.state.transcribe_duration = 0.0;
                                    this.state.finish_active_queue_item(Err(err));
                                } else {
                                    this.state.status = ProcessStatus::Failed(err);
                                }
                                if this.state.batch_running {
                                    this.advance_batch_queue(cx);
                                }
                            }
                        }
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// 通用导出流程：编辑落库 -> 弹系统保存对话框 -> 后台写出文件 -> 状态回写。
    /// 统一四个导出入口的重复骨架（对话框参数与写出函数因格式而异）。
    pub(crate) fn export_with_save_dialog(
        &mut self,
        cx: &mut Context<Self>,
        default_name: String,
        filter_label: &str,
        filter_ext: String,
        write_file: impl FnOnce(&[crate::subtitle::Segment], &std::path::Path) -> anyhow::Result<()>
            + Send
            + 'static,
    ) {
        if self.state.segments.is_empty() {
            return;
        }
        self.state.flush_segments_if_dirty();

        let segments = self.state.segments.clone();
        let filter_label = filter_label.to_string();
        cx.spawn(async move |this, cx| {
            let handle = rfd::AsyncFileDialog::new()
                .set_file_name(&default_name)
                .add_filter(&filter_label, &[filter_ext.as_str()])
                .save_file()
                .await;
            if let Some(save_handle) = handle {
                let save_path = save_handle.path().to_path_buf();
                let res = write_file(&segments, &save_path);
                let _ = this.update(cx, |this, cx| {
                    match res {
                        Ok(()) => {
                            info!("字幕成功导出至: {:?}", save_path);
                            // 成功必须给出反馈：此前这里只有一行日志，界面上毫无动静。
                            // 用户点了「导出」、选了路径、然后**什么都没有发生**——
                            // 无法区分「成功」与「没点到」，只能去文件管理器里翻。
                            // 走中性提示条（notice）而不是错误条：这是成功操作。
                            this.notice = Some(format!(
                                "已导出 {} 句字幕到 {}",
                                segments.len(),
                                save_path
                                    .file_name()
                                    .map(|n| n.to_string_lossy().to_string())
                                    .unwrap_or_else(|| save_path.display().to_string())
                            ));
                        }
                        Err(e) => {
                            // 导出失败也要走跨页可见的提示：`ProcessStatus::Failed` 只有
                            // 转写页的底部状态栏会渲染，而导出是在剪辑台做的。
                            this.state.status = ProcessStatus::Failed(format!("导出失败: {}", e));
                            this.notice = None;
                        }
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// 剪映专业版草稿一键直接注入（自动写入本机剪映草稿库，打开剪映即可见）
    pub(crate) fn export_jianying_local(&mut self, cx: &mut Context<Self>) {
        if self.state.segments.is_empty() {
            return;
        }
        self.state.flush_segments_if_dirty();

        let segments = self.state.segments.clone();
        // 条数先取出来：`segments` 会被 move 进后台任务，成功提示还要用它
        let seg_count = segments.len();
        // 工程文件按「导出模式」（原文 / 仅译文 / 双语）写入；有译文时压成单行。
        let mode = self.editor_export_mode;
        let video_path = self.state.selected_file.clone();
        let stem = self
            .state
            .selected_file
            .as_ref()
            .and_then(|p| p.file_stem())
            .and_then(|s| s.to_str())
            .unwrap_or("Voice2Word")
            .to_string();

        cx.spawn(async move |this, cx| {
            let res = cx
                .background_executor()
                .spawn(async move {
                    crate::subtitle::JianYingExporter::inject_to_local_jianying(
                        &segments,
                        video_path.as_deref(),
                        &stem,
                        mode,
                    )
                })
                .await;

            let _ = this.update(cx, |this, cx| {
                match res {
                    Ok(draft_path) => {
                        info!("成功注入剪映草稿: {:?}", draft_path);
                        let _ = std::process::Command::new("explorer")
                            .arg(&draft_path)
                            .spawn();
                        this.state.status = ProcessStatus::Idle;
                        // 已经打开了资源管理器，但仍给一条提示说明「注入了什么」：
                        // 只弹窗不说明的话，用户不知道这是导入成功还是碰巧打开了目录。
                        this.notice = Some(format!(
                            "已注入剪映草稿（{seg_count} 句字幕），打开剪映即可在首页看到"
                        ));
                    }
                    Err(e) => {
                        tracing::error!("剪映草稿注入失败: {:?}", e);
                        this.state.status =
                            ProcessStatus::Failed(format!("剪映草稿注入失败: {}", e));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 导出剪映草稿至指定独立文件夹
    pub(crate) fn export_jianying_folder(&mut self, cx: &mut Context<Self>) {
        if self.state.segments.is_empty() {
            return;
        }

        let segments = self.state.segments.clone();
        let mode = self.editor_export_mode;
        let video_path = self.state.selected_file.clone();
        let stem = self
            .state
            .selected_file
            .as_ref()
            .and_then(|p| p.file_stem())
            .and_then(|s| s.to_str())
            .unwrap_or("Voice2Word")
            .to_string();

        cx.spawn(async move |this, cx| {
            let handle = rfd::AsyncFileDialog::new()
                .set_title("选择剪映草稿保存目录")
                .pick_folder()
                .await;
            if let Some(folder_handle) = handle {
                let target_dir = folder_handle.path().join(&stem);
                let res = cx
                    .background_executor()
                    .spawn(async move {
                        crate::subtitle::JianYingExporter::export_to_folder(
                            &segments,
                            video_path.as_deref(),
                            &target_dir,
                            &stem,
                            mode,
                        )
                    })
                    .await;

                let _ = this.update(cx, |this, cx| {
                    match res {
                        Ok(p) => {
                            info!("剪映草稿文件夹导出成功: {:?}", p);
                            let _ = std::process::Command::new("explorer").arg(&p).spawn();
                            this.notice = Some(format!(
                                "已导出剪映草稿到 {}",
                                p.file_name()
                                    .map(|n| n.to_string_lossy().to_string())
                                    .unwrap_or_else(|| p.display().to_string())
                            ));
                        }
                        Err(e) => {
                            this.state.status =
                                ProcessStatus::Failed(format!("剪映草稿导出失败: {}", e));
                        }
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// 导出 FCPXML (Final Cut Pro / 达芬奇) 工程文件
    pub(crate) fn export_fcpxml(&mut self, cx: &mut Context<Self>) {
        if self.state.segments.is_empty() {
            return;
        }
        let stem = self
            .state
            .selected_file
            .as_ref()
            .and_then(|p| p.file_stem())
            .and_then(|s| s.to_str())
            .unwrap_or("subtitle")
            .to_string();

        let mode = self.editor_export_mode;
        self.export_with_save_dialog(
            cx,
            crate::subtitle::writer::export_file_name(
                &self.state.config.ui.export_name_template,
                &stem,
                "fcpxml",
                &chrono::Local::now().format("%Y%m%d").to_string(),
            ),
            "Final Cut Pro XML (*.fcpxml)",
            "fcpxml".to_string(),
            move |segs, path| {
                crate::subtitle::FcpXmlExporter::write_to_file(segs, path, &stem, mode)
            },
        );
    }

    /// 导出 Adobe Premiere Pro XML (FCP7 XML / xmeml) 工程文件
    pub(crate) fn export_premiere_xml(&mut self, cx: &mut Context<Self>) {
        if self.state.segments.is_empty() {
            return;
        }
        let stem = self
            .state
            .selected_file
            .as_ref()
            .and_then(|p| p.file_stem())
            .and_then(|s| s.to_str())
            .unwrap_or("subtitle")
            .to_string();

        let mode = self.editor_export_mode;
        self.export_with_save_dialog(
            cx,
            crate::subtitle::writer::export_file_name(
                &self.state.config.ui.export_name_template,
                &stem,
                "xml",
                &chrono::Local::now().format("%Y%m%d").to_string(),
            ),
            "Premiere Pro XML (*.xml)",
            "xml".to_string(),
            move |segs, path| {
                crate::subtitle::PremiereXmlExporter::write_to_file(segs, path, &stem, mode)
            },
        );
    }

    /// 使用 FFplay 播放当前视频，并通过 subtitles 滤镜叠加已生成字幕。
    pub(crate) fn play_video(&mut self, cx: &mut Context<Self>) {
        let Some(input_file) = self.state.selected_file.clone() else {
            return;
        };
        if self.state.segments.is_empty() {
            self.state.status = ProcessStatus::Failed("请先完成字幕识别，再播放预览".to_string());
            cx.notify();
            return;
        }
        self.state.flush_segments_if_dirty();

        let subtitle_path =
            std::env::temp_dir().join(format!("voice2word_preview_{}.srt", std::process::id()));
        // 复用同一临时文件名，写新前先清旧，避免历次预览 SRT 在 temp 无限累积
        let _ = std::fs::remove_file(&subtitle_path);
        // FFplay 弹窗预览与主界面/导出同源：按当前导出内容模式生成预览字幕。
        let preview_mode = self.editor_export_mode;
        // 预览与最终导出同源：带上配置里的字幕样式，按 `max_chars_per_line` 折行。
        // ffplay 的 `subtitles` 滤镜按 SRT 规范渲染多行 cue，折行对它是**正向**的
        // （预览看到的分行就是导出得到的分行）；不折反而会让「预览正常、导出超屏」。
        let preview_style = self.state.config.subtitle_style.clone();
        if let Err(error) = SubtitleWriter::write_srt_with_style(
            &self.state.segments,
            &subtitle_path,
            preview_mode,
            &preview_style,
        ) {
            self.state.status = ProcessStatus::Failed(format!("生成预览字幕失败: {}", error));
            cx.notify();
            return;
        }

        let ffmpeg_path = crate::utils::AppConfig::resolve_path(&self.state.config.paths.ffmpeg);
        let ffplay_path = ffmpeg_path.with_file_name("ffplay.exe");
        // Run ffplay from the temp directory and pass only the SRT filename.
        // This avoids Windows drive-letter colons being parsed as filter options.
        let subtitle_name = subtitle_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("voice2word_preview.srt");
        let cur_time = format!("{:.3}", self.state.current_time);
        // 预览播放器是「发起后没人 wait」的短命进程：句柄一丢就成孤儿进程，
        // 而 `-autoexit` 只在「正常播完」时生效——用户在播放中关窗、或反复点预览，
        // 都会在系统里留下没人回收的 ffplay.exe（占着视频文件句柄与音频设备）。
        // 因此：先收掉上一次预览（避免堆积），再把新进程登记到全局表，
        // 由 main.rs 在应用退出时统一 kill + wait。
        crate::utils::child_registry::retire_all();
        match std::process::Command::new(ffplay_path)
            .arg("-ss")
            .arg(&cur_time)
            .arg("-loglevel")
            .arg("error")
            .arg("-window_title")
            .arg("Voice2Word 视频字幕预览")
            .arg("-vf")
            .arg(format!("subtitles=filename='{subtitle_name}'"))
            .arg("-autoexit")
            .arg(input_file)
            .current_dir(std::env::temp_dir())
            .spawn()
        {
            Ok(child) => {
                // 先并入全局作业对象，再登记：这样「本进程被强杀」时 ffplay 也会
                // 被系统一并清掉，而不仅是靠退出路径里的 retire_all。
                crate::utils::child_registry::adopt(&child);
                crate::utils::child_registry::register(child);
                info!("已启动带字幕视频预览");
            }
            Err(error) => {
                self.state.status = ProcessStatus::Failed(format!("启动视频预览失败: {}", error));
                cx.notify();
            }
        }
    }

    /// 请求终止当前转写任务：标记取消并强杀识别子进程。
    ///
    /// 状态先切到「终止中」给用户即时反馈，真正的复位由管线的收尾事件统一完成
    /// （见 `start_processing` 里 `cancel_requested` 的分支），避免两处各写一套收尾。
    /// 未在转写时调用是安全的空操作。
    pub(crate) fn request_cancel_processing(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.state.status, ProcessStatus::Processing { .. }) {
            return;
        }
        self.state.cancel_requested = true;
        self.state.status = ProcessStatus::Processing {
            stage: "终止中".to_string(),
            progress: 1.0,
            detail: "正在终止识别进程，请稍候...".to_string(),
        };
        self.state.pipeline.cancel();
        cx.notify();
    }

    /// 异步探测当前视频的真实分辨率（驱动监视器画面等比适配，杜绝字幕掉进黑边、画面拉伸变形）
    pub(crate) fn probe_video_dimensions(&mut self, cx: &mut Context<Self>) {
        let Some(video) = self.state.preview_media().cloned() else {
            return;
        };
        if self.state.dims_probed_for.as_ref() == Some(&video) {
            return;
        }
        self.state.dims_probed_for = Some(video.clone());
        let ffmpeg_path = crate::utils::AppConfig::resolve_path(&self.state.config.paths.ffmpeg);
        let ffmpeg = crate::engines::FFmpegEngine::new(ffmpeg_path);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { ffmpeg.get_resolution(&video) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Some((w, h)) = result {
                    this.state.video_width = w;
                    this.state.video_height = h;
                    this.state.video_player.set_frame_dimensions(w, h);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// 异步提取当前工程的音频波形包络，驱动时间轴波形轨。
    ///
    /// 与 `probe_video_dimensions` 同样是「按媒体路径去重」的惰性任务：
    /// 来源一致直接复用已有包络；来源变了先清空再重算，避免时间轴在新波形
    /// 到达前画出上一支视频的包络（时间刻度完全对不上）。
    pub(crate) fn ensure_waveform(&mut self, cx: &mut Context<Self>) {
        let Some(media) = self.state.preview_media().cloned() else {
            return;
        };
        if !media.is_file() {
            return;
        }
        if self.state.waveform_ready_for(&media) {
            return;
        }
        if self.state.waveform_busy_for.as_ref() == Some(&media) {
            return;
        }

        self.state.waveform = None;
        self.state.waveform_busy_for = Some(media.clone());

        let ffmpeg = crate::engines::FFmpegEngine::new(crate::utils::AppConfig::resolve_path(
            &self.state.config.paths.ffmpeg,
        ));
        let buckets = crate::engines::waveform::MAX_WAVEFORM_BUCKETS;
        let extract_target = media.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    crate::engines::waveform::extract(&ffmpeg, &extract_target, buckets)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.state.waveform_busy_for.as_ref() == Some(&media) {
                    this.state.waveform_busy_for = None;
                }
                match result {
                    Ok(data) => {
                        info!(buckets = data.peaks.len(), "时间轴波形包络提取完成");
                        this.state.waveform = Some(std::sync::Arc::new(data));
                    }
                    // 波形只是辅助视觉，失败就静默不显示，绝不能因此打断转写主流程
                    Err(error) => {
                        tracing::warn!(error = %error, "波形提取失败，时间轴将不显示波形轨");
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 异步根据当前播放时间抽取单帧画面（单飞队列：最多 1 个 FFmpeg 实例并发，合并多余拖动请求）
    pub(crate) fn trigger_extract_frame(&mut self, cx: &mut Context<Self>) {
        self.probe_video_dimensions(cx);
        let Some(video_path) = self.state.preview_media().cloned() else {
            return;
        };
        if !video_path.exists() {
            return;
        }
        let target_time = self.state.current_time;

        // 记录最新期望的时间戳
        {
            let mut pending = self.pending_extract_time.lock().unwrap();
            *pending = Some(target_time);
        }

        // 如果当前已有后台抽帧 worker 在运行，直接返回！运行中的 worker 抽完后会自动接取 pending_time
        if self
            .is_extracting_frame
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
            )
            .is_err()
        {
            return;
        }

        let ffmpeg_path = crate::utils::AppConfig::resolve_path(&self.state.config.paths.ffmpeg);
        let ffmpeg = std::sync::Arc::new(crate::engines::FFmpegEngine::new(ffmpeg_path));
        let cache = self.state.frame_cache.clone();
        let is_extracting = self.is_extracting_frame.clone();
        let pending_time = self.pending_extract_time.clone();

        cx.spawn(async move |this, cx| {
            loop {
                // 取出当前最新的目标时间
                let next_time = {
                    let mut lock = pending_time.lock().unwrap();
                    lock.take()
                };

                let time = match next_time {
                    Some(t) => t,
                    None => {
                        // 无待处理请求，释放锁并退出
                        is_extracting.store(false, std::sync::atomic::Ordering::SeqCst);
                        // 双重检查避免竞态退出
                        let has_more = pending_time.lock().unwrap().is_some();
                        if has_more
                            && is_extracting
                                .compare_exchange(
                                    false,
                                    true,
                                    std::sync::atomic::Ordering::SeqCst,
                                    std::sync::atomic::Ordering::SeqCst,
                                )
                                .is_ok()
                        {
                            continue;
                        }
                        break;
                    }
                };

                let video_clone = video_path.clone();
                let ffmpeg_clone = ffmpeg.clone();
                let cache_clone = cache.clone();

                let frame_result =
                    cx.background_executor()
                        .spawn(async move {
                            cache_clone.get_or_extract(&video_clone, time, &ffmpeg_clone)
                        })
                        .await;

                let is_latest = {
                    let lock = pending_time.lock().unwrap();
                    lock.is_none()
                };

                // 仅当当前抽取结果依然是最新位置时才提交 UI 渲染，杜绝旧帧闪现与滞后延迟感
                if is_latest {
                    if let Ok(frame_path) = frame_result {
                        let _ = this.update(cx, |this, cx| {
                            // 在这里判一次存在性：`get_or_extract` 正常返回的路径都已落盘，
                            // 但磁盘帧目录会按「最近使用」裁剪——把存在性收敛到后台更新点，
                            // 渲染路径就不必每帧对预览帧 `path.exists()`。
                            this.state.preview_frame_path = if frame_path.exists() {
                                Some(frame_path)
                            } else {
                                None
                            };
                            cx.notify();
                        });
                    }
                }
            }
        })
        .detach();
    }

    pub(crate) fn halt_preview_playback(&mut self) {
        if self.state.is_playing {
            self.state.current_time = self.state.video_player.current_play_time();
            self.state.is_playing = false;
        }
        self.state.flush_segments_if_dirty();
        self.state.video_player.stop();
        self.play_tick_generation = self.state.video_player.generation();
    }

    /// 上一句字幕
    pub(crate) fn jump_prev_segment(&mut self, cx: &mut Context<Self>) {
        if self.state.segments.is_empty() {
            return;
        }
        self.halt_preview_playback();
        let cur_idx = self.state.selected_segment_index.unwrap_or(1);
        let new_idx = if cur_idx > 1 { cur_idx - 1 } else { 1 };
        self.state.select_segment(new_idx);
        self.trigger_extract_frame(cx);
        cx.notify();
    }

    /// 下一句字幕
    pub(crate) fn jump_next_segment(&mut self, cx: &mut Context<Self>) {
        if self.state.segments.is_empty() {
            return;
        }
        self.halt_preview_playback();
        let cur_idx = self.state.selected_segment_index.unwrap_or(1);
        let new_idx = if cur_idx < self.state.segments.len() {
            cur_idx + 1
        } else {
            self.state.segments.len()
        };
        self.state.select_segment(new_idx);
        self.trigger_extract_frame(cx);
        cx.notify();
    }

    /// 切换内嵌实时播放模式 (25fps 动态图像 + FFplay 同步伴音)
    pub(crate) fn toggle_play_preview(&mut self, cx: &mut Context<Self>) {
        if self.state.is_playing {
            // 暂停：先记下墙钟时间，保留最后一帧，避免闪黑或抽帧滞后。
            self.state.current_time = self.state.video_player.current_play_time();
            // 暂停点落在切句边界上时，选中句与画面字幕也要一起落在同一句
            // （判据同 `get_active_segment`）；此刻已停播，编辑缓冲可安全对齐。
            self.state.sync_selection_to_time(self.is_text_focused);
            self.state.is_playing = false;
            self.state.video_player.pause_clock();
            self.trigger_extract_frame(cx);
            cx.notify();
        } else {
            let Some(video_file) = self.state.preview_media().cloned() else {
                return;
            };
            self.state.flush_segments_if_dirty();
            self.state.is_playing = true;
            let start_sec = self.state.current_time;
            self.state.video_player.play(video_file, start_sec);
            let tick_gen = self.state.video_player.generation();
            self.play_tick_generation = tick_gen;

            let player = self.state.video_player.clone();
            cx.spawn(async move |this, cx| loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(40))
                    .await;
                let should_continue = this
                    .update(cx, |this, cx| {
                        if this.play_tick_generation != tick_gen {
                            return false;
                        }
                        if !this.state.is_playing {
                            return false;
                        }
                        let play_time = player.current_play_time();
                        let max_dur = if this.state.total_duration > 0.0 {
                            this.state.total_duration
                        } else {
                            this.state.segments.last().map(|s| s.end).unwrap_or(0.0)
                        };

                        if play_time >= max_dur && max_dur > 0.0 {
                            this.state.current_time = max_dur;
                            this.state.is_playing = false;
                            this.state.video_player.pause_clock();
                            this.trigger_extract_frame(cx);
                            cx.notify();
                            return false;
                        }

                        this.state.current_time = play_time;
                        // 选中句 / 编辑缓冲跟随画面字幕。判据与监视器覆盖层
                        // 同源（`get_active_segment` 的半开区间），否则在切句边界上，
                        // 右侧列表跳到下一句、画面还在上一句，看着就是对不上。
                        // 用户正在编辑框里打字时不动编辑缓冲，避免冲掉输入。
                        this.state.sync_selection_to_time(this.is_text_focused);
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);

                if !should_continue {
                    break;
                }
            })
            .detach();
            cx.notify();
        }
    }

    /// 根据当前剪辑工作台选中的格式执行统一导出
    pub(crate) fn perform_editor_export(&mut self, cx: &mut Context<Self>) {
        self.state.flush_segments_if_dirty();
        if self.state.segments.is_empty() {
            return;
        }

        let stem = self
            .state
            .selected_file
            .as_ref()
            .and_then(|p| p.file_stem())
            .and_then(|s| s.to_str())
            .unwrap_or("subtitle")
            .to_string();

        match self.editor_export_format {
            EditorExportFormat::JianYing => self.export_jianying_local(cx),
            EditorExportFormat::JianYingFolder => self.export_jianying_folder(cx),
            EditorExportFormat::Fcpxml => self.export_fcpxml(cx),
            EditorExportFormat::PremiereXml => self.export_premiere_xml(cx),
            EditorExportFormat::Srt
            | EditorExportFormat::Ass
            | EditorExportFormat::Txt
            | EditorExportFormat::Vtt
            | EditorExportFormat::Json
            | EditorExportFormat::EbuTtD
            | EditorExportFormat::NetflixTtal => {
                // 枚举先归一成格式字符串（`ui/mod.rs` 不动），再交给
                // `export_spec_for` 求扩展名与对话框过滤器名——这样这两份
                // 字面量只存在于 `writer.rs` 一处，不会再与 writer 的分派表漂移。
                let fmt = editor_export_format_string(self.editor_export_format);
                let (ext, filter_label) = export_spec_for(fmt);
                // 直接按目标扩展名导出，不再临时改写全局 output_format（避免副作用泄漏到后续管线调用）
                // ASS 会带上主界面配置的字幕样式（字号/字间距/底边距/预设配色）
                let style = self.state.config.subtitle_style.clone();
                let mode = self.editor_export_mode;
                self.export_with_save_dialog(
                    cx,
                    crate::subtitle::writer::export_file_name(
                        &self.state.config.ui.export_name_template,
                        &stem,
                        ext,
                        &chrono::Local::now().format("%Y%m%d").to_string(),
                    ),
                    filter_label,
                    ext.to_string(),
                    move |segs, path| {
                        SubtitleWriter::write_to_file_with_style(segs, path, ext, mode, &style)
                    },
                );
            }
        }
    }

    /// 触发大模型多语言字幕后台流式翻译。
    ///
    /// 引擎按 `state.translate_mode` 二选一：本地 Qwen（llama.cpp）或在线
    /// OpenAI 兼容接口；两条链路的进度回调协议一致，UI 侧无需分支。
    pub(crate) fn trigger_llm_translation(&mut self, cx: &mut Context<Self>) {
        if self.state.segments.is_empty() || self.state.is_translating {
            return;
        }
        // 「已完成」必须按**当前目标语言**判定。用不分语言的 `translated_count()`
        // 会踩到这个坑：用户把目标语言从 English 改成 日本語 后，旧英文译文仍算
        // 「已完成」，于是 100% 时这里直接 return——点「开始翻译」毫无反应，
        // 用户完全不知道为什么。现在只有「当前语言全部译完」才提前返回。
        // 另：这里与收尾文案用**同一套**统计口径，避免“按钮说已完成、收尾又说部分完成”自相矛盾。
        let target_now = self.state.translate_target_lang.clone();
        // 「已完成」用**实际译出**口径：分子只认非空译文 + 目标语言匹配，
        // 分母只算源文非空的句子（空源句没有可译内容，计入分母会永远差几句）。
        let done_now = translated_out_count(&self.state.segments, &target_now);
        let expected_now = expected_translation_count(&self.state.segments);
        if expected_now == 0 {
            self.state.translate_status_msg = "没有可翻译的文本（所有字幕行都是空的）".to_string();
            cx.notify();
            return;
        }
        if done_now >= expected_now {
            self.state.translate_status_msg =
                format!("{} 句已全部翻译为{}", expected_now, target_now);
            cx.notify();
            return;
        }

        let mode = self.state.translate_mode;
        self.state.is_translating = true;
        self.state.translate_progress = 0.0;
        self.state.translate_status_msg = match mode {
            crate::engines::TranslateMode::OnlineApi => {
                format!(
                    "正在初始化在线翻译引擎 ({})...",
                    self.state.config.translate.api_model
                )
            }
            crate::engines::TranslateMode::OfflineQwen => "正在初始化 Qwen 翻译引擎...".to_string(),
        };
        cx.notify();

        let segments = self.state.segments.clone();
        let target_lang = self.state.translate_target_lang.clone();
        // 另存一份给收尾统计用：`target_lang` 会被 move 进下面的异步块。
        // 收尾必须按本轮目标语言数：否则用户换语言后，旧语言的译文
        // 会被当成本轮成果，界面又会说「已译出 N 句」而实际一句没译。
        let target_lang_for_msg = target_lang.clone();
        let online_cfg = self.state.online_translate_config();
        let llama_cli =
            crate::utils::config::AppConfig::resolve_path(&self.state.config.paths.llama_cli);
        let llm_model =
            crate::utils::config::AppConfig::resolve_path(&self.state.config.paths.llm_model);
        let llm_ctx = self.state.config.pipeline.llm_ctx;
        let llm_threads = self.state.config.pipeline.llm_threads;
        // 术语表提示：离线链路要显式注入到 LLM 引擎（在线链路已随 online_cfg 一起带）。
        let glossary_hint = self.state.translate_glossary_hint();
        // 取消标志：任务起手先复位（上一轮取消后它会停在 true），再交给引擎
        let cancel_flag = self.translate_cancel.clone();
        cancel_flag.store(false, std::sync::atomic::Ordering::SeqCst);

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(f64, String)>();

        cx.spawn(async move |this, cx| {
            let handle = cx.background_executor().spawn(async move {
                let translate_engine = match mode {
                    crate::engines::TranslateMode::OnlineApi => {
                        crate::engines::TranslateEngine::online(online_cfg)
                    }
                    crate::engines::TranslateMode::OfflineQwen => {
                        let llm_engine = crate::engines::LLMEngine::new(
                            llama_cli,
                            llm_model,
                            llm_ctx,
                            llm_threads,
                        );
                        llm_engine.set_glossary(&glossary_hint);
                        crate::engines::TranslateEngine::offline(llm_engine)
                    }
                };
                let tx_clone = tx.clone();
                translate_engine.translate_subtitles(
                    segments,
                    &target_lang,
                    Some(Box::new(move |p, msg| {
                        let _ = tx_clone.send((p, msg.to_string()));
                    })),
                    cancel_flag,
                )
            });

            let progress_this = this.clone();
            cx.spawn(async move |cx| {
                while let Some((progress, msg)) = rx.recv().await {
                    let _ = progress_this.update(cx, |this, cx| {
                        this.state.translate_progress = progress as f32;
                        this.state.translate_status_msg = msg;
                        cx.notify();
                    });
                }
            })
            .detach();

            let result = handle.await;
            // 取消位要在 update 闭包里读、在闭包里复位——中间不能被别的任务改掉
            let was_cancelled = this
                .update(cx, |this, cx| {
                    let c = this
                        .translate_cancel
                        .load(std::sync::atomic::Ordering::SeqCst);
                    this.state.is_translating = false;
                    this.translate_cancel
                        .store(false, std::sync::atomic::Ordering::SeqCst);
                    cx.notify();
                    c
                })
                .unwrap_or(false);
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(translated_segs) => {
                        // 收尾统计用「实际译出」口径，而不是「引擎有没有给语言标签」：
                        // 引擎解析失败时可能写下空串/纯空白译文（照样带 translation_lang），
                        // 只按标签计数就会把一片空白显示成「翻译完成」。
                        // `translation_coverage` 的分子只认 `has_translation()`（忽略空串/纯空白），
                        // 分母只算源文非空的句子。
                        let (done, expected) =
                            translation_coverage(&translated_segs, &target_lang_for_msg);
                        // 引擎在取消时返回**已完成的偏序结果**而非错误，所以这里必须
                        // 显式区分「跑完了」与「被取消了」。不区分的话，用户点了取消
                        // 却看到「翻译完成（N 句）」，会以为取消没生效。
                        if was_cancelled {
                            info!("字幕多语言翻译已取消（已完成 {} 句）", done);
                            this.state.translate_status_msg =
                                format!("已取消翻译（保留已完成的 {} 句）", done);
                        } else if expected == 0 || done >= expected {
                            // 全部译出（或整片都是空源句， expected == 0，本轮无遗留）
                            info!("字幕多语言翻译完成（{}/{} 句带译文）", done, expected);
                            this.state.translate_progress = 1.0;
                            this.state.translate_status_msg = format!("翻译完成（{} 句）", done);
                        } else if done == 0 {
                            // 一句都没译出：引擎本该返 Err（见 engines 的 fail-fast），
                            // 这里再兜一层——只要没有任何实际译文就绝不能显示「完成」，
                            // 否则就是「点了翻译、界面说成功、字幕全是空白」的假成功。
                            let msg = format!(
                                "翻译失败：应有 {expected} 句译文，但引擎未译出任何一句。请检查翻译引擎配置（在线模式看地址/密钥/模型名，离线模式看 Qwen 模型与 llama.cpp）后重试。"
                            );
                            tracing::warn!("{}", msg);
                            // 统一收口（见 AppState::note_translate_failure）：转写进行中
                            // 时只写翻译面板，绝不覆盖转写进度态。
                            this.state.note_translate_failure(msg);
                        } else {
                            // 部分译出：说清「差多少」，并明确可以再点一次补齐——
                            // 引擎是增量翻译，不会把已译好的重译一遍。
                            let missing = expected - done;
                            info!(
                                "字幕多语言翻译部分完成（{}/{} 句，{} 句未译出）",
                                done, expected, missing
                            );
                            this.state.translate_progress = done as f32 / expected as f32;
                            this.state.translate_status_msg = format!(
                                "翻译部分完成：{done} / {expected} 句已译出，剩余 {missing} 句未译出（可再次点击「开始翻译」补齐）"
                            );
                        }
                        // 译文**按句合并**回当前字幕表，而不是整表覆盖。
                        //
                        // 翻译要跑几分钟，而这段时间里用户仍能在剪辑台编辑字幕
                        // （改错别字、调时间、拆合句）。引擎拿的是开始时的快照，
                        // 若在这里 `= translated_segs` 直接覆盖，用户这几分钟的
                        // 编辑会被静默回滚——改了半天，翻译一结束全没了。
                        // `merge_translations` 只把译文按 index 并回去，原文/时间
                        // 等用户改动原样保留。
                        this.state.merge_translations(translated_segs);
                        // 译文必须落库，否则重启后历史库里的双语对照会凭空消失
                        this.state.segments_dirty = true;
                        this.state.flush_segments_if_dirty();
                    }
                    Err(e) => {
                        // 引擎在「一句都没译出」（密钥/地址/模型名错、Qwen 起不来）时
                        // 会返回带真实原因的 Err（见 engines 的 fail-fast），这里把它
                        // 失败统一走 `note_translate_failure`：它在没有转写在跑时才占用
                        // 全局 `ProcessStatus::Failed` 横幅，避免翻译失败把转写进度态冲掉。
                        tracing::warn!("字幕多语言翻译过程报错: {}", e);
                        let msg = format!("翻译失败：{e}");
                        // 统一收口（见 AppState::note_translate_failure）：转写进行中
                        // 时只写翻译面板，绝不覆盖转写进度态。
                        this.state.note_translate_failure(msg);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 取消正在进行的字幕翻译。
    ///
    /// 只置位取消标志：引擎在**每个批次之间**检查（离线 Qwen 与在线 API 两条链路
    /// 都接了），因此最多损失当前一批，已翻译的句子原样保留。任务收尾时统一复位
    /// （见 `trigger_llm_translation` 的 `handle.await` 之后）。
    pub(crate) fn cancel_llm_translation(&mut self, cx: &mut Context<Self>) {
        if !self.state.is_translating {
            return;
        }
        self.translate_cancel
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.state.translate_status_msg = "正在取消翻译，等待当前批次结束…".to_string();
        cx.notify();
    }

    /// 清空当前字幕列表中的全部译文，恢复单语状态。
    pub(crate) fn clear_all_translations(&mut self, cx: &mut Context<Self>) {
        if self.state.is_translating {
            self.notice = Some("翻译正在进行中，请先取消翻译再清空译文".to_string());
            cx.notify();
            return;
        }
        let had_trans = self.state.segments.iter().any(|s| s.translation.is_some());
        if !had_trans {
            self.notice = Some("当前没有译文可清空".to_string());
            cx.notify();
            return;
        }
        for seg in &mut self.state.segments {
            seg.translation = None;
            seg.translation_lang = None;
        }
        self.state.segments_dirty = true;
        self.state.flush_segments_if_dirty();
        self.notice = Some("已清空全部译文，已恢复为单语显示".to_string());
        cx.notify();
    }

    /// 打开（或创建）术语表临时文件，供用户用记事本等编辑器编辑。
    ///
    /// 术语表是多行文本，自绘单行输入框既装不下也没法用输入法舒服编辑；改为写文件 +
    /// 系统编辑器打开，用户改完回来点「应用术语表」读回。
    pub(crate) fn open_glossary_editor(&mut self, cx: &mut Context<Self>) {
        match self.state.write_glossary_temp_file() {
            Ok(path) => {
                // 用系统默认编辑器打开（记事本 / 用户关联的 .txt 程序）
                let _ = std::process::Command::new("cmd")
                    .arg("/C")
                    .arg("start")
                    .arg("")
                    .arg(&path)
                    .spawn();
                self.glossary_status = Some(
                    "已打开软件目录下的术语表（glossary.txt）。改完保存后点「应用术语表」。"
                        .to_string(),
                );
            }
            Err(e) => {
                self.glossary_status = Some(format!("无法创建术语表文件: {e}"));
            }
        }
        cx.notify();
    }

    /// 从术语表临时文件读回内容并落盘。
    pub(crate) fn apply_glossary_from_file(&mut self, cx: &mut Context<Self>) {
        match self.state.reload_glossary_from_temp_file() {
            Ok(n) => {
                // 同样按**实际生效**数报：`glossary_prompt` 只注入前
                // `MAX_GLOSSARY_ENTRIES` 条，超限部分静默丢弃。
                // 这里说「共 N 条生效」而实际只生效 80 条就是对用户撒谎。
                let limit = self.state.config.translate.effective_glossary_limit();
                let effective = super::views::performance::effective_glossary_count(n, limit);
                if effective < n {
                    self.glossary_status = Some(format!(
                        "已应用术语表：共 {n} 条，其中 {effective} 条生效（受上限 {limit} 条限制）"
                    ));
                } else {
                    self.glossary_status = Some(format!("已应用术语表，共 {n} 条生效"));
                }
            }
            Err(e) => {
                self.glossary_status = Some(format!(
                    "读取术语表失败: {e}（请先点「编辑术语表」创建文件）"
                ));
            }
        }
        cx.notify();
    }

    /// 在线翻译接口连通性自检（设置页「测试连接」按钮）
    pub(crate) fn probe_online_translate_api(&mut self, cx: &mut Context<Self>) {
        if self.is_probing_translate {
            return;
        }
        if self.state.config.translate.api_model.trim().is_empty() {
            self.translate_probe_msg = Some((false, "请先填写模型名".to_string()));
            cx.notify();
            return;
        }
        if self
            .state
            .config
            .translate
            .effective_api_key()
            .trim()
            .is_empty()
        {
            self.translate_probe_msg = Some((
                false,
                "请先填写 API Key（或设置环境变量 VOICE2WORD_API_KEY）".to_string(),
            ));
            cx.notify();
            return;
        }

        self.is_probing_translate = true;
        self.translate_probe_msg = Some((true, "正在连接在线接口...".to_string()));
        cx.notify();

        let cfg = self.state.online_translate_config();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { crate::engines::TranslateEngine::online(cfg).probe_online() })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.is_probing_translate = false;
                this.translate_probe_msg = Some(match result {
                    Ok(reply) => (true, format!("连接成功，模型回显: {}", reply.trim())),
                    Err(e) => (false, format!("连接失败: {e}")),
                });
                cx.notify();
            });
        })
        .detach();
    }
}

// ─────────── 模型下载（首次使用引导）───────────

impl MainWindow {
    /// 下载一个模型 / 组件。
    ///
    /// 在**后台线程**执行：最大 833 MB 的文件在慢网下要几十分钟，绝不能在
    /// UI 线程上跑。进度通过 `mpsc` 回传，取消用一个 `AtomicBool`
    /// （与翻译链路同一套模式）。
    pub(crate) fn start_model_download(&mut self, item_id: &'static str, cx: &mut Context<Self>) {
        if self.state.is_downloading {
            return;
        }
        let Some(item) = crate::utils::ITEMS.iter().find(|i| i.id == item_id) else {
            self.state.download_status_msg = format!("未知的下载项: {item_id}");
            cx.notify();
            return;
        };

        self.state.is_downloading = true;
        self.state.download_current = Some(item_id.to_string());
        self.state.download_done_bytes = 0;
        self.state.download_total_bytes = 0;
        self.state.download_status_msg = format!("正在下载 {}...", item.label);
        self.model_download_cancel
            .store(false, std::sync::atomic::Ordering::SeqCst);
        cx.notify();

        let cancel = self.model_download_cancel.clone();
        let (tx, mut rx) = mpsc::unbounded_channel::<(u64, u64, String)>();

        // 进度回传协程
        cx.spawn(async move |this, cx| {
            while let Some((done, total, id)) = rx.recv().await {
                let _ = this.update(cx, |this, cx| {
                    this.state.download_done_bytes = done;
                    this.state.download_total_bytes = total;
                    this.state.download_current = Some(id);
                    cx.notify();
                });
            }
        })
        .detach();

        let item = *item;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let cb: crate::utils::model_download::ProgressFn =
                        Box::new(move |done, total, id| {
                            let _ = tx.send((done, total, id.to_string()));
                        });
                    crate::utils::model_download::download_one(&item, &cancel, Some(&cb))
                })
                .await;

            let _ = this.update(cx, |this, cx| {
                this.state.is_downloading = false;
                this.state.download_current = None;
                this.state.download_done_bytes = 0;
                this.state.download_total_bytes = 0;
                match result {
                    Ok(path) => {
                        info!("模型下载成功: {}", path.display());
                        // 压缩包类组件（如 llama.cpp）多一步解压，提示语要区分开，
                        // 否则用户以为只下了个包、不知道程序已经到位可用了。
                        let verb = if item.is_archive {
                            "下载并解压完成"
                        } else {
                            "下载完成"
                        };
                        this.state.download_status_msg = format!("{} {verb}", item.label);
                        if item.id == "whisper-cublas" {
                            this.state.config.paths.whisper_cli = item.dest.to_string();
                            let _ = this.state.config.save_to_file("config.toml");
                        }
                        // 重新扫描：新文件就位后界面上的「缺失」标记要立即消失
                        this.state.refresh_model_presence();
                        // 模型换了，硬件档案里的推荐配置与预览代理可能要重算
                        this.state.refresh_hardware_detection();
                    }
                    Err(err) => {
                        let msg = err.to_string();
                        tracing::warn!(error = %err, item = item.id, "模型下载失败");
                        this.state.download_status_msg = if msg.contains("已取消") {
                            format!("{} 下载已取消", item.label)
                        } else {
                            format!("{} 下载失败: {msg}", item.label)
                        };
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 一键补齐所有**缺失**的条目（按体积从小到大，先让用户看到进展）。
    pub(crate) fn download_all_missing(&mut self, cx: &mut Context<Self>) {
        if self.state.is_downloading {
            return;
        }
        let current_tab = self.model_manager_tab;
        let mut pending: Vec<&'static crate::utils::DownloadItem> = crate::utils::ITEMS
            .iter()
            .filter(|i| !self.state.model_is_present(i.id))
            .filter(|i| {
                crate::ui::components::model_manager::is_item_needed_for_tab(
                    i.id,
                    current_tab,
                    &self.state,
                )
            })
            .collect();
        // 小文件先下：几十 MB 的 VAD/tokens 几秒就完，用户立刻看到进展；
        // ffmpeg(50MB) 与主模型(180MB+) 排在后面。
        pending.sort_by_key(|i| i.size);
        if pending.is_empty() {
            self.state.download_status_msg = match current_tab {
                crate::ui::ModelManagerTab::SenseVoice => "SenseVoice 所需组件均已就位".to_string(),
                crate::ui::ModelManagerTab::Whisper => "Whisper 所需组件均已就位".to_string(),
                crate::ui::ModelManagerTab::Common => "公用支撑组件均已就位".to_string(),
            };
            cx.notify();
            return;
        }

        // 条目总数在循环前取好：`pending` 会被 for 消费掉，
        // 收尾提示还要用它算「完成 N/M」。
        let total = pending.len();
        let cancel = self.model_download_cancel.clone();
        cancel.store(false, std::sync::atomic::Ordering::SeqCst);
        self.state.is_downloading = true;
        self.state.download_status_msg = format!("正在补齐 {total} 个缺失组件...");
        cx.notify();

        let (tx, mut rx) = mpsc::unbounded_channel::<(u64, u64, String)>();
        cx.spawn(async move |this, cx| {
            while let Some((done, total_b, id)) = rx.recv().await {
                let _ = this.update(cx, |this, cx| {
                    this.state.download_done_bytes = done;
                    this.state.download_total_bytes = total_b;
                    this.state.download_current = Some(id);
                    cx.notify();
                });
            }
        })
        .detach();

        cx.spawn(async move |this, cx| {
            // 逐个串行下载：并发下多个大文件抢带宽，反而都变慢，
            // 且进度条会来回跳（用户无法判断「还剩多少」）。
            let mut ok = 0usize;
            let mut failed: Vec<&'static str> = Vec::new();
            let mut last_msg = String::new();
            for item in pending {
                if cancel.load(std::sync::atomic::Ordering::SeqCst) {
                    last_msg = "批量下载已取消".to_string();
                    break;
                }
                let tx_cb = tx.clone();
                let cancel_cb = cancel.clone();
                let item_copy = *item;
                let r = cx
                    .background_executor()
                    .spawn(async move {
                        let cb: crate::utils::model_download::ProgressFn =
                            Box::new(move |done, tot, id| {
                                let _ = tx_cb.send((done, tot, id.to_string()));
                            });
                        crate::utils::model_download::download_one(
                            &item_copy,
                            &cancel_cb,
                            Some(&cb),
                        )
                    })
                    .await;
                match r {
                    Ok(_) => ok += 1,
                    Err(e) => {
                        tracing::warn!(item = item.id, error = %e, "批量下载中该项失败");
                        failed.push(item.label);
                    }
                }
            }
            let _ = this.update(cx, |this, cx| {
                this.state.is_downloading = false;
                this.state.download_current = None;
                this.state.download_done_bytes = 0;
                this.state.download_total_bytes = 0;
                this.state.refresh_model_presence();
                this.state.refresh_hardware_detection();
                this.state.download_status_msg = if !last_msg.is_empty() {
                    last_msg
                } else if failed.is_empty() {
                    format!("全部 {total} 个组件下载完成")
                } else {
                    format!("完成 {ok}/{total}；失败：{}", failed.join("、"))
                };
                cx.notify();
            });
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::{editor_export_format_string, export_spec_for};
    use crate::ui::EditorExportFormat;

    /// 回归（重构护栏）：`perform_editor_export` 里原本内联着两份 match
    /// （枚举 -> 扩展名、枚举 -> 对话框过滤器名）。收敛成
    /// `editor_export_format_string` + `export_spec_for` 之后，这里逐档断言
    /// **结果与重构前的字面量完全一致**，确保这次去重没有改变任何用户可见行为。
    #[test]
    fn editor_export_spec_matches_previous_inline_literals() {
        for (fmt, ext, label) in [
            (EditorExportFormat::Srt, "srt", "SRT 字幕"),
            (EditorExportFormat::Ass, "ass", "ASS 特效字幕"),
            (EditorExportFormat::Txt, "txt", "TXT 纯文本"),
            (EditorExportFormat::Vtt, "vtt", "VTT 网页字幕"),
            (EditorExportFormat::Json, "json", "JSON 结构化字幕"),
            (EditorExportFormat::EbuTtD, "ttml", "EBU-TT-D 广播字幕"),
            (EditorExportFormat::NetflixTtal, "ttal", "Netflix TTAL 字幕"),
        ] {
            let spec = export_spec_for(editor_export_format_string(fmt));
            assert_eq!(spec.0, ext, "{fmt:?} 的扩展名变了");
            assert_eq!(spec.1, label, "{fmt:?} 的对话框过滤器名变了");
        }
    }

    /// 上层的 `match` 已经替剪映 / FCPXML / Premiere XML 分流，它们不会走到
    /// 单文件导出；万一漏进来，也必须落在一个 writer 一定写得出来的格式上，
    /// 而不是 `unreachable!()` 把 UI 线程打挂。
    #[test]
    fn non_single_file_formats_degrade_to_srt_instead_of_panicking() {
        for fmt in [
            EditorExportFormat::JianYing,
            EditorExportFormat::JianYingFolder,
            EditorExportFormat::Fcpxml,
            EditorExportFormat::PremiereXml,
        ] {
            assert_eq!(
                editor_export_format_string(fmt),
                "srt",
                "{fmt:?} 应回落 srt"
            );
        }
    }

    /// 回归（P2）：收尾统计必须用「实际译出」口径——
    /// 引擎可能给空串/纯空白译文打上 `translation_lang`，只数标签就会把
    /// 空白显示成「翻译完成」；分母还必须排除空源句，否则会永远「部分完成」。
    #[test]
    fn translation_coverage_ignores_blank_translations_and_empty_sources() {
        use crate::subtitle::Segment;
        let mut with_blank = Segment::new(1, 0.0, 1.0, "你好");
        with_blank.translation = Some("   ".to_string());
        with_blank.translation_lang = Some("English".to_string()); // 脏标签：有标签无译文
        let mut good = Segment::new(2, 1.0, 2.0, "世界");
        good.translation = Some("World".to_string());
        good.translation_lang = Some("English".to_string());
        let empty_source = Segment::new(3, 2.0, 3.0, "   ");
        let missing = Segment::new(4, 3.0, 4.0, "未译");
        let segs = vec![with_blank, good, empty_source, missing];
        // 分子：只有 good 算译出（空串被忽略）；分母： 3 句有源文，空源句不计。
        assert_eq!(super::translation_coverage(&segs, "English"), (1, 3));
        // 换目标语言：旧译文不能算成本轮成果（否则又会出现「换语言后显示已译出」）
        assert_eq!(super::translation_coverage(&segs, "日本語"), (0, 3));
        assert_eq!(super::translated_out_count(&segs, "English"), 1);
    }
}
