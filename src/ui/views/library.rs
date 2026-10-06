//! 媒体库与历史任务视图

use gpui::prelude::*;
use gpui::*;
use std::path::PathBuf;
use std::sync::Arc;

use crate::app::state::{ProcessStatus, WorkspaceTab};
use crate::subtitle::SubtitleWriter;
use crate::utils::time::format_duration_short;
use super::super::primitives;
use super::super::theme::Theme;
use super::super::types::{ConfirmAction, ConfirmDialogInfo};
use super::super::MainWindow;

impl MainWindow {
    /// 渲染历史视频库 (视频资产管理与一键载入工作台)
    pub(crate) fn render_library_layout(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let total_count = self.state.recent_tasks.len();

        // 为缺失首帧的卡片后台派发缩略图提取（幂等，完成后自动刷新）
        self.ensure_library_thumbnails(cx);
        // 同理补齐文件大小（每任务只 stat 一次，不再逐帧读盘）
        self.ensure_library_sizes(cx);

        // 页面外壳统一走 primitives::page_shell：内边距 / 分区间距与其余工作台页同源。
        // 注意本页内容自身带滚动列表（library-cards-scroll），外层不再额外滚动，
        // 故把 page_shell 自带的 overflow_y_scroll 顶掉，避免双滚动条。
        primitives::page_shell("library-workspace-layout")
            .overflow_hidden()
            // 顶部标头栏
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(Theme::SPACE_3))
                            .child(primitives::page_title("视频库"))
                            // 计数徽标统一走 badge 原语（不再是手写的圆角胶囊）
                            .child(primitives::badge(format!("{} 项", total_count))),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(Theme::SPACE_2))
                            .child(
                                primitives::btn_clickable("导入视频", primitives::BtnSize::Md, primitives::BtnVariant::Primary)
                                    .id("library-import-btn")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.state.active_tab = WorkspaceTab::Generate;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                primitives::btn_clickable("刷新", primitives::BtnSize::Md, primitives::BtnVariant::Secondary)
                                    .id("library-refresh-btn")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.state.refresh_recent_tasks();
                                        // 列表可能因刷新而变短，顺带裁掉查不到的缓存
                                        this.prune_library_caches();
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
            // 视频卡片列表区域
            .child(
                if total_count == 0 {
                    self.render_library_empty(cx)
                } else {
                    self.render_library_cards(cx)
                }
            )
    }

    /// 空状态
    fn render_library_empty(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(Theme::SPACE_3))
            .child(
                div()
                    .text_size(px(Theme::TEXT_HEADING))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(Theme::text_secondary())
                    .child("暂无解析历史"),
            )
            .child(
                div()
                    .text_size(px(Theme::TEXT_BODY_LG))
                    .text_color(Theme::text_muted())
                    .child("导入视频文件后，转写记录将在此显示"),
            )
            .child(
                // 空态主行动按钮走 btn 原语，与其余页面的 CTA 同高同色
                primitives::btn_clickable("导入视频开始转写", primitives::BtnSize::Lg, primitives::BtnVariant::Primary)
                    .id("empty-lib-goto-gen")
                    .mt(px(Theme::SPACE_2))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.state.active_tab = WorkspaceTab::Generate;
                        cx.notify();
                    })),
            )
            .into_any_element()
    }

    /// 卡片列表：直接借用 recent_tasks，避免每次渲染克隆全部任务的 segments
    fn render_library_cards(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("library-cards-scroll")
            .flex_1()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(Theme::SPACE_3))
            .children(self.state.recent_tasks.iter().map(|task| {
                self.render_library_card(task, cx)
            }))
            .into_any_element()
    }

    /// 为视频库卡片补齐「文件大小」显示文本（每个任务只探测一次）。
    ///
    /// 大小要 `fs::metadata`，而卡片渲染每帧都读它；列表又没有虚拟化，
    /// 40 条记录 = 每帧 40 次 stat。因此在载入视频库时探一次并缓存。
    ///
    /// 探测放在后台线程：文件可能在机械盘或网络盘上，`metadata` 卡住时
    /// 不能拖住 UI 线程。失败（文件被删/不可访问）写「未知大小」占位，
    /// 与缩略图失败占位同一策略，避免每次渲染重复派发。
    fn ensure_library_sizes(&mut self, cx: &mut Context<Self>) {
        let pending: Vec<(i64, PathBuf)> = self
            .state
            .recent_tasks
            .iter()
            .filter(|t| !self.library_sizes.contains_key(&t.id))
            .map(|t| (t.id, PathBuf::from(&t.file_path)))
            .collect();
        if pending.is_empty() {
            return;
        }

        cx.spawn(async move |this, cx| {
            let computed = cx
                .background_executor()
                .spawn(async move {
                    pending
                        .into_iter()
                        .map(|(id, path)| {
                            let text = std::fs::metadata(&path)
                                .ok()
                                .map(|m| format_file_size(m.len()))
                                .unwrap_or_else(|| "未知大小".to_string());
                            (id, text)
                        })
                        .collect::<Vec<_>>()
                })
                .await;

            let _ = this.update(cx, |this, cx| {
                for (id, text) in computed {
                    this.library_sizes.insert(id, text);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 为视频库卡片异步预提取首帧缩略图。
    /// 复用全局 FrameCache（磁盘级缓存：路径+文件签名+时间键，跨会话命中），时间 0.0 即首帧；
    /// 每个任务仅派发一次，失败时写入空路径标记防止逐帧重试刷爆后台。
    fn ensure_library_thumbnails(&mut self, cx: &mut Context<Self>) {
        const MAX_THUMB_WORKERS: usize = 2;
        // 先收集待派发项（只取 id 与路径），避免在借用 recent_tasks 的同时
        // 修改 library_thumb_inflight，也避免克隆整段字幕数据
        let pending: Vec<(i64, PathBuf)> = self
            .state
            .recent_tasks
            .iter()
            .filter(|t| {
                !self.library_thumbs.contains_key(&t.id)
                    && !self.library_thumb_inflight.contains(&t.id)
            })
            .map(|t| (t.id, PathBuf::from(&t.file_path)))
            .collect();

        for (task_id, video_path) in pending {
            // A render can revisit this method frequently. Bound background
            // ffmpeg work so a large library cannot starve Whisper workers.
            if self.library_thumb_inflight.len() >= MAX_THUMB_WORKERS {
                break;
            }
            if !video_path.exists() {
                continue;
            }
            self.library_thumb_inflight.insert(task_id);

            let ffmpeg = Arc::new(crate::engines::FFmpegEngine::new(
                crate::utils::AppConfig::resolve_path(&self.state.config.paths.ffmpeg),
            ));
            let cache = self.state.frame_cache.clone();

            cx.spawn(async move |this, cx| {
                let thumb = cx
                    .background_executor()
                    .spawn(async move { cache.get_or_extract(&video_path, 0.0, &ffmpeg) })
                    .await;

                let _ = this.update(cx, |this, cx| {
                    this.library_thumb_inflight.remove(&task_id);
                    // 失败（或后台提取出来的文件随后被磁盘裁剪掉）都归一成**空路径**，
                    // 空路径 = 「没有可用缩略图」，渲染时直接走占位框且不再重复派发。
                    // 在这里判一次存在性（后台线程），渲染路径就不必每帧对每张卡片
                    // `path.exists()` —— 库列表没有虚拟化，40 张卡片就是每帧 40 次 stat。
                    let raw = thumb.unwrap_or_default();
                    let usable = if raw.as_os_str().is_empty() || raw.exists() {
                        raw
                    } else {
                        PathBuf::new()
                    };
                    this.library_thumbs.insert(task_id, usable);
                    cx.notify();
                });
            })
            .detach();
        }
    }

    /// 单条视频卡片 —— 横向布局：缩略图 | 信息区 | 操作按钮
    /// 只借用任务记录，点击时再按 id 回查，避免每次渲染克隆整段字幕
    fn render_library_card(
        &self,
        task: &crate::storage::db::TaskRecord,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let task_id = task.id;
        let seg_len = task.segment_count;
        let dur_str = format_duration_short(task.duration);

        // 文件扩展名
        let ext = task.file_name
            .rsplit('.')
            .next()
            .unwrap_or("MP4")
            .to_uppercase();

        // 文件大小：查**缓存快照**，不在这里 stat。
        //
        // 这个函数每个可见卡片每帧都会被调一次，而视频库列表是普通
        // `overflow_y_scroll` + `children`（没有虚拟化），40 条记录就是
        // 每帧 40 次 `fs::metadata` —— 磁盘慢或文件在网络盘上时直接表现为
        // 滚动卡顿。大小只在「载入视频库」时探测一次（见 `ensure_library_sizes`）。
        let file_size_str = self
            .library_sizes
            .get(&task_id)
            .cloned()
            .unwrap_or_else(|| "未知大小".to_string());

        // 摘录首条字幕文本 (按字符边界截断，避免切进多字节中文字符导致 panic)
        // 列表查询已由 SQLite 抽出首句文本，这里不再反序列化整段字幕
        let sample_text = {
            let t = task.sample_text.clone();
            if t.trim().is_empty() {
                "无字幕内容".to_string()
            } else if t.chars().count() > 40 {
                let cut: String = t.chars().take(40).collect();
                format!("{}...", cut)
            } else {
                t
            }
        };

        // 格式化日期（取日期部分）
        let date_display = if task.created_at.len() >= 10 {
            task.created_at[..10].to_string()
        } else {
            task.created_at.clone()
        };

        div()
            .id(("lib-card", task_id as usize))
            .w_full()
            .p_4()
            .rounded_xl()
            .bg(Theme::bg_card())
            .border_1()
            .border_color(Theme::border())
            .hover(|s| s.border_color(Theme::border_light()).bg(Theme::bg_card_hover()))
            .flex()
            .flex_row()
            .gap_4()
            // ── 左侧：首帧缩略图（后台提取完成前显示占位） ──
            .child({
                let thumb_box = div()
                    .flex_shrink_0()
                    .w(px(Theme::LIB_THUMB_W))
                    .h(px(Theme::LIB_THUMB_H))
                    .rounded_lg()
                    .overflow_hidden()
                    .bg(Theme::bg_sidebar())
                    .border_1()
                    .border_color(Theme::border_subtle())
                    .relative();

                match self.library_thumbs.get(&task_id) {
                    Some(path) if !path.as_os_str().is_empty() => thumb_box
                        // 视频首帧铺满卡片，Cover 裁剪对齐 16:9
                        .child(
                            img(path.clone())
                                .size_full()
                                .object_fit(ObjectFit::Cover),
                        )
                        // 底部信息条：格式 + 时长
                        .child(
                            div()
                                .absolute()
                                .bottom_0()
                                .left_0()
                                .right_0()
                                .flex()
                                .items_center()
                                .justify_between()
                                .px_2()
                                .py_0p5()
                                // 角标条压在缩略图上，底色恒为深色，文字必须用媒体区浅色 token
                                // （浅色主题下 text_muted 会变深，压在深底上直接糊掉）
                                .bg(Theme::bg_overlay())
                                .child(
                                    div()
                                        .text_size(px(Theme::TEXT_CAPTION))
                                        .font_weight(FontWeight::BOLD)
                                        .text_color(Theme::text_on_overlay())
                                        .child(ext),
                                )
                                .child(
                                    div()
                                        .text_size(px(Theme::TEXT_CAPTION))
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(Theme::text_on_overlay())
                                        .child(dur_str.clone()),
                                ),
                        ),
                    _ => thumb_box
                        .flex()
                        .items_center()
                        .justify_center()
                        .flex_col()
                        .gap_1()
                        // 格式标识
                        .child(
                            div()
                                .px_3()
                                .py_1()
                                .rounded_md()
                                .bg(Theme::bg_raised())
                                .border_1()
                                .border_color(Theme::border_mid())
                                .text_size(px(Theme::TEXT_BODY_LG))
                                .font_weight(FontWeight::BOLD)
                                .text_color(Theme::text_muted())
                                .child(ext),
                        )
                        // 时长标签
                        .child(
                            div()
                                .text_size(px(Theme::TEXT_SMALL))
                                .text_color(Theme::text_muted())
                                .child(dur_str.clone()),
                        ),
                }
            })
            // ── 中间：信息区 ──
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .justify_between()
                    .gap_2p5()
                    // 文件名 + 状态
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2p5()
                                    // 文件名 (大字)
                                    .child(
                                        div()
                                            .text_size(px(Theme::TEXT_HEADING))
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(Theme::text_primary())
                                            .overflow_hidden()
                                            .child(task.file_name.clone()),
                                    )
                                    // 已完成标记
                                    .child(
                                        primitives::stat_dot(Theme::accent_mint()),
                                    ),
                            )
                            // 元数据标签行
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    // 时长标签
                                    .child(Self::render_meta_pill("时长", &dur_str))
                                    // 文件大小
                                    .child(Self::render_meta_pill("大小", &file_size_str))
                                    // 字幕段数
                                    .child(Self::render_meta_pill("字幕", &format!("{} 句", seg_len)))
                                    // 处理耗时 (如果有 metrics)
                                    .children(task.metrics.as_ref().map(|m| {
                                        Self::render_meta_pill("耗时", &format!("{:.1}s", m.total_elapsed_sec))
                                    })),
                            ),
                    )
                    // 字幕预览文本
                    .child(
                        div()
                            .text_size(px(Theme::TEXT_BODY_LG))
                            .text_color(Theme::text_muted())
                            .overflow_hidden()
                            .child(sample_text),
                    )
                    // 日期
                    .child(
                        div()
                            .text_size(px(Theme::TEXT_BODY))
                            .text_color(Theme::text_muted())
                            .child(date_display),
                    ),
            )
            // ── 右侧：操作按钮组 ──
            .child(
                div()
                    .flex_shrink_0()
                    .flex()
                    .flex_col()
                    .items_end()
                    .justify_center()
                    .gap_2()
                    // 耗时详情 (if metrics)
                    .children(task.metrics.clone().map(|m| {
                        let fname = task.file_name.clone();
                        // 次级薄荷按钮：薄荷浅底 + 薄荷字，与主操作的实心薄荷区分开
                        primitives::btn_mint_soft("耗时详情")
                            .id(("lib-metrics-btn", task_id as usize))
                            .w(px(Theme::LIB_ACTION_W))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.benchmark_dialog = Some(crate::ui::types::BenchmarkDialogInfo {
                                    file_name: fname.clone(),
                                    metrics: m.clone(),
                                });
                                cx.notify();
                            }))
                    }))
                    // 剪辑按钮 (主操作)
                    .child(
                        // 主操作：实心薄荷方角按钮，与全站 CTA 同形
                        primitives::btn_clickable("剪辑校对", primitives::BtnSize::Md, primitives::BtnVariant::Primary)
                            .id(("lib-edit-btn", task_id as usize))
                            .w(px(Theme::LIB_ACTION_W))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                // 点击时才按 id 取出完整任务数据（渲染期不再克隆）
                                let Some(record) = this
                                    .state
                                    .recent_tasks
                                    .iter()
                                    .find(|t| t.id == task_id)
                                    .cloned()
                                else {
                                    return;
                                };
                                this.state.load_task(&record);
                                this.trigger_extract_frame(cx);
                                this.ensure_preview_proxy(cx);
                                this.ensure_waveform(cx);
                                cx.notify();
                            })),
                    )
                    // 导出按钮：次级中性按钮（抬升底 + 描边），方角
                    .child(
                        primitives::btn_clickable("导出字幕", primitives::BtnSize::Md, primitives::BtnVariant::Secondary)
                            .id(("lib-export-btn", task_id as usize))
                            .w(px(Theme::LIB_ACTION_W))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                // 同上：仅在真正导出时才取出该任务的字幕数据
                                let Some(record) = this
                                    .state
                                    .recent_tasks
                                    .iter()
                                    .find(|t| t.id == task_id)
                                    .cloned()
                                else {
                                    return;
                                };
                                let task_name = record.file_name.clone();
                                // 列表记录不含字幕正文，导出时按 id 现取，避免为一次导出
                                // 把整库字幕都留在内存里
                                let segs = this
                                    .state
                                    .db
                                    .load_task_segments(task_id)
                                    .unwrap_or_default();
                                // 与剪辑台同源：尊重用户在导出栏选的「原文 / 仅译文 / 双语」。
                                // 此前这里写死用 `write_to_file` 自动判定，用户改成「仅译文」
                                // 后从历史库导出仍会带上原文，两处行为不一致。
                                let export_mode = this.state.export_mode_from_config();
                                cx.spawn(async move |this, cx| {
                                    if let Some(handle) = rfd::AsyncFileDialog::new()
                                        .set_file_name(&format!("{}.srt", task_name))
                                        .add_filter("SubRip Subtitle", &["srt"])
                                        .save_file()
                                        .await
                                    {
                                        let save_path = handle.path().to_path_buf();
                                        if let Err(err) = SubtitleWriter::write_to_file_with_mode(
                                            &segs,
                                            &save_path,
                                            "srt",
                                            export_mode,
                                        ) {
                                            // 导出失败必须报出来：静默吞掉会让用户以为
                                            // 文件已经写出去了，回头找不到又无从排查。
                                            let _ = this.update(cx, |this, cx| {
                                                this.state.status = ProcessStatus::Failed(format!(
                                                    "导出失败: {err}"
                                                ));
                                                cx.notify();
                                            });
                                        }
                                    }
                                })
                                .detach();
                            })),
                    )
                    // 删除按钮：危险操作按钮原语，卡片级尺寸（32px 圆角块）
                    .child({
                        let task_name = task.file_name.clone();
                        primitives::btn_danger("删除", primitives::BtnSize::Lg)
                            .id(("lib-del-btn", task_id as usize))
                            .w(px(Theme::LIB_ACTION_W))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                // 删除不可逆，且若删的是当前工程会静默换掉工作区，
                                // 必须先确认。真正执行走 ConfirmAction。
                                this.confirm_dialog = Some(ConfirmDialogInfo {
                                    title: "删除这条记录？".to_string(),
                                    message: format!(
                                        "「{task_name}」及其字幕将被永久删除，无法恢复。"
                                    ),
                                    confirm_label: "删除".to_string(),
                                    danger: true,
                                    action: ConfirmAction::DeleteTaskRecord(task_id),
                                });
                                cx.notify();
                            }))
                    }),
            )
    }

    /// 元数据标签 (圆角小药丸)：标签名 + 值
    fn render_meta_pill(label: &str, value: &str) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .gap_1p5()
            .px_2p5()
            .py_1()
            .rounded_md()
            .bg(Theme::bg_raised())
            .border_1()
            .border_color(Theme::border_subtle())
            .child(
                div()
                    .text_size(px(Theme::TEXT_SMALL))
                    .text_color(Theme::text_muted())
                    .child(format!("{}:", label)),
            )
            .child(
                div()
                    .text_size(px(Theme::TEXT_BODY))
                    .font_weight(FontWeight::BOLD)
                    .text_color(Theme::text_secondary())
                    .child(value.to_string()),
            )
    }
}

/// 把字节数格式化成界面用的体积文本。
///
/// 抽成独立函数是因为它现在跑在后台线程里（见 `ensure_library_sizes`），
/// 与渲染路径解耦，也便于单测。
fn format_file_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else {
        format!("{:.0} KB", b / KB)
    }
}

#[cfg(test)]
mod tests {
    use super::format_file_size;

    #[test]
    fn file_size_formatting_matches_previous_rendering() {
        // 与迁移前的渲染逻辑逐字对齐，避免「顺手重构」改变界面观感
        assert_eq!(format_file_size(1_073_741_824), "1.0 GB");
        assert_eq!(format_file_size(2_147_483_648), "2.0 GB");
        assert_eq!(format_file_size(1_048_576), "1.0 MB");
        assert_eq!(format_file_size(66_359), "65 KB");
        assert_eq!(format_file_size(1024), "1 KB");
        assert_eq!(format_file_size(0), "0 KB");
    }
}
