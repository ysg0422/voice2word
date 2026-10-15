//! 底部状态与操作栏部件

use gpui::prelude::*;
use gpui::*;

use super::super::primitives;
use super::super::theme::Theme;
use super::super::MainWindow;
use crate::app::state::ProcessStatus;

/// 底部状态栏主按钮的文案。
///
/// 抽成纯函数是为了能单测：这段判定有五个分支，写在渲染闭包里就只能靠肉眼比对；
/// 而它决定的恰恰是「按钮说自己在做什么」——文案与真实动作对不上，用户会以为
/// 按下它会发生别的事。
///
/// 口径与右侧转写配置抽屉的主 CTA 完全一致（「开始转写 / 开始全部」，进行中一律
/// 带「转写」二字）：同一件事不该在底部工具栏里换个说法。
fn start_button_label(
    is_processing: bool,
    batch_running: bool,
    has_file: bool,
    has_batch: bool,
) -> &'static str {
    if is_processing {
        if batch_running {
            // 批量跑的是队列，说「转写中」会让用户以为只有当前这一条在跑
            "批量转写中…"
        } else {
            "转写中…"
        }
    } else if has_file {
        "开始转写"
    } else if has_batch {
        "开始全部"
    } else {
        // 无文件也无队列：按钮本来就是禁用态，这里给的是「补上文件后它会做什么」
        "开始转写"
    }
}

/// 失败摘要最多显示的字符数（按**字符**而非字节计）。
///
/// 42 个全角字符约为状态栏右区被按钮挤占后剩下的宽度；再宽就会顶到操作按钮。
const FAILURE_SUMMARY_MAX_CHARS: usize = 42;

/// 错误串为空（或纯空白）时的兜底文案：宁可直说「没给原因」，也不能显示成空白——
/// 空白会被读成「没出错」。
const FAILURE_SUMMARY_PLACEHOLDER: &str = "出错（未提供原因）";

/// 悬停浮层的最大宽度：错误串常常很长（模型路径、引擎 stderr），限宽让它换行显示，
/// 而不是拉成一条横贯屏幕的长线。
const FAILURE_TOOLTIP_MAX_W: f32 = 360.0;

/// 把失败原因压成状态栏里放得下的**一行摘要**（纯函数，便于单测）。
///
/// 规则：
/// - 换行 / 制表 / 连续空白折叠成单个空格：状态栏只有一行高，留着换行会把栏位撑高；
/// - 空串或纯空白给 `FAILURE_SUMMARY_PLACEHOLDER`，避免看起来像没出错；
/// - 超过 `FAILURE_SUMMARY_MAX_CHARS` 个字符时**按字符**截断并补「…」。按字节切
///   （如 `String::truncate`）会把中文 / emoji 劈成半个码位，渲染成乱码方块。
fn failure_summary(full: &str) -> String {
    let mut flat = String::with_capacity(full.len());
    let mut pending_space = false;
    for ch in full.chars() {
        if ch.is_whitespace() {
            // 前导空白直接吞掉，避免摘要以空格开头
            pending_space = !flat.is_empty();
        } else {
            if pending_space {
                flat.push(' ');
                pending_space = false;
            }
            flat.push(ch);
        }
    }

    if flat.is_empty() {
        return FAILURE_SUMMARY_PLACEHOLDER.to_string();
    }

    if flat.chars().count() <= FAILURE_SUMMARY_MAX_CHARS {
        return flat;
    }

    // 用 `chars()` 取整字符，天然落在 UTF-8 字符边界上
    let kept: String = flat.chars().take(FAILURE_SUMMARY_MAX_CHARS).collect();
    format!("{kept}…")
}

/// 状态栏失败摘要的悬停浮层。
///
/// gpui 的 `tooltip` 只接受 `AnyView`（须是 `Render` 实体），没有「挂一个 title 字符串」
/// 这种轻量入口，所以这里放一个最小视图承载**完整**错误串：状态栏只放得下一行摘要，
/// 但用户要判断下一步怎么做，得看全。
struct FailureTooltip {
    /// 完整错误串（不截断），与状态栏摘要同源。
    text: String,
}

impl Render for FailureTooltip {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .max_w(px(FAILURE_TOOLTIP_MAX_W))
            .px(px(Theme::SPACE_2))
            .py(px(Theme::SPACE_1))
            .rounded(px(Theme::RADIUS_SM))
            .bg(Theme::bg_raised())
            .border_1()
            .border_color(Theme::border_strong())
            .shadow_lg()
            .text_size(px(Theme::TEXT_SMALL))
            .text_color(Theme::text_primary())
            .child(self.text.clone())
    }
}

impl MainWindow {
    /// 全局失败提示条。
    ///
    /// `ProcessStatus::Failed` 由 16 处产生（导出失败、预览启动失败、剪映注入失败、
    /// 未识别出字幕……），但**底部状态栏只挂在「语音转写」页**（`views/transcribe.rs`）。
    /// 用户在剪辑台点「导出」失败、在视频库删记录失败时，错误串确实写进了
    /// `state.status`，却没有任何控件能显示它——界面一片安静，用户只会认为按钮坏了。
    ///
    /// 这里把失败态提升为一条**跨页常驻**的错误条，挂在标题栏正下方。点「知道了」
    /// 把状态复位为 `Idle`（与「文件已就绪」等常规态一致），错误串随之消失。
    /// 只渲染 `Failed` 一种，不影响其余状态在转写页的正常展示。
    /// 字幕写库失败横幅。
    ///
    /// 单列一条而不是塞进 `state.status`：那个状态表示「转写失败了」，
    /// 与「改动没能存盘」是两件互不相干的事——转写成功、但编辑保存失败，
    /// 是完全可能的组合。而且这条要**常驻可见**直到用户处理，不能因为
    /// 一次转写状态变化就被覆盖掉。
    pub(crate) fn render_db_error_banner(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(msg) = self.state.db_write_error.clone() else {
            return div().into_any_element();
        };

        div()
            .id("db-error-banner")
            .w_full()
            .flex_shrink_0()
            .px(px(Theme::PAGE_PAD))
            .py(px(Theme::SPACE_2))
            .bg(Theme::tint_red_soft())
            .border_b_1()
            .border_color(Theme::tint_red_border())
            .flex()
            .items_center()
            .gap(px(Theme::SPACE_2))
            .child(primitives::stat_dot_sm(Theme::accent_red()))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .text_size(px(Theme::TEXT_BODY))
                    .text_color(Theme::accent_red())
                    // 说明后果，而不只是报错：用户需要知道「现在关程序会丢东西」
                    .child(format!(
                        "{msg}（改动尚未写入历史库，请检查磁盘空间或文件权限）"
                    )),
            )
            .child(
                div()
                    .id("db-error-banner-dismiss")
                    .flex_shrink_0()
                    .px(px(Theme::SPACE_2))
                    .py(px(Theme::SPACE_0_5))
                    .rounded(px(Theme::RADIUS_SM))
                    .cursor_pointer()
                    .text_size(px(Theme::TEXT_SMALL))
                    .text_color(Theme::accent_red())
                    .hover(|s| s.bg(Theme::tint_red_border()))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.state.db_write_error = None;
                        cx.notify();
                    }))
                    .child("知道了"),
            )
            .into_any_element()
    }

    /// 设置落盘失败横幅。
    ///
    /// 与 [`Self::render_db_error_banner`] 并列的另一种「没保存上」：这里指
    /// `config.toml` 写失败（磁盘满、目录只读、文件被占用），后果是**用户改的
    /// 设置重启后失效**。单列一条而不是并进 db 横幅，是因为两者可以同时发生，
    /// 且处置提示不同（一个写历史库、一个写配置文件）。
    pub(crate) fn render_config_error_banner(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(msg) = self.state.config_write_error.clone() else {
            return div().into_any_element();
        };

        div()
            .id("config-error-banner")
            .w_full()
            .flex_shrink_0()
            .px(px(Theme::PAGE_PAD))
            .py(px(Theme::SPACE_2))
            .bg(Theme::tint_red_soft())
            .border_b_1()
            .border_color(Theme::tint_red_border())
            .flex()
            .items_center()
            .gap(px(Theme::SPACE_2))
            .child(primitives::stat_dot_sm(Theme::accent_red()))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .text_size(px(Theme::TEXT_BODY))
                    .text_color(Theme::accent_red())
                    // 说明后果：用户需要知道「重启后这次改动会丢」
                    .child(format!(
                        "{msg}（设置尚未写入 config.toml，重启后可能失效，请检查磁盘空间或文件权限）"
                    )),
            )
            .child(
                div()
                    .id("config-error-banner-dismiss")
                    .flex_shrink_0()
                    .px(px(Theme::SPACE_2))
                    .py(px(Theme::SPACE_0_5))
                    .rounded(px(Theme::RADIUS_SM))
                    .cursor_pointer()
                    .text_size(px(Theme::TEXT_SMALL))
                    .text_color(Theme::accent_red())
                    .hover(|s| s.bg(Theme::tint_red_border()))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.state.config_write_error = None;
                        cx.notify();
                    }))
                    .child("知道了"),
            )
            .into_any_element()
    }

    pub(crate) fn render_error_banner(&self, cx: &mut Context<Self>) -> AnyElement {
        let ProcessStatus::Failed(msg) = &self.state.status else {
            return div().into_any_element();
        };

        let msg_for_copy = msg.clone();
        div()
            .id("global-error-banner")
            .w_full()
            .flex_shrink_0()
            .px(px(Theme::PAGE_PAD))
            .py(px(Theme::SPACE_2))
            .bg(Theme::tint_red_soft())
            .border_b_1()
            .border_color(Theme::tint_red_border())
            .flex()
            .items_center()
            .gap(px(Theme::SPACE_2))
            .child(primitives::stat_dot_sm(Theme::accent_red()))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .text_size(px(Theme::TEXT_BODY))
                    .text_color(Theme::accent_red())
                    .child(msg.clone()),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(Theme::SPACE_1_5))
                    .child(
                        div()
                            .id("global-error-banner-view-log")
                            .flex_shrink_0()
                            .px(px(Theme::SPACE_2))
                            .py(px(Theme::SPACE_0_5))
                            .rounded(px(Theme::RADIUS_SM))
                            .bg(Theme::bg_inset())
                            .border_1()
                            .border_color(Theme::tint_red_border())
                            .cursor_pointer()
                            .text_size(px(Theme::TEXT_SMALL))
                            .text_color(Theme::accent_red())
                            .hover(|s| s.bg(Theme::tint_red_border()).text_color(Theme::text_primary()))
                            .on_click(cx.listener(|_this, _, _, _cx| {
                                crate::utils::open_latest_log_file();
                            }))
                            .child("查看报错日志"),
                    )
                    .child(
                        div()
                            .id("global-error-banner-open-dir")
                            .flex_shrink_0()
                            .px(px(Theme::SPACE_2))
                            .py(px(Theme::SPACE_0_5))
                            .rounded(px(Theme::RADIUS_SM))
                            .bg(Theme::bg_inset())
                            .border_1()
                            .border_color(Theme::tint_red_border())
                            .cursor_pointer()
                            .text_size(px(Theme::TEXT_SMALL))
                            .text_color(Theme::accent_red())
                            .hover(|s| s.bg(Theme::tint_red_border()).text_color(Theme::text_primary()))
                            .on_click(cx.listener(|_this, _, _, _cx| {
                                crate::utils::open_logs_folder();
                            }))
                            .child("打开日志目录"),
                    )
                    .child(
                        div()
                            .id("global-error-banner-copy")
                            .flex_shrink_0()
                            .px(px(Theme::SPACE_2))
                            .py(px(Theme::SPACE_0_5))
                            .rounded(px(Theme::RADIUS_SM))
                            .bg(Theme::bg_inset())
                            .border_1()
                            .border_color(Theme::tint_red_border())
                            .cursor_pointer()
                            .text_size(px(Theme::TEXT_SMALL))
                            .text_color(Theme::accent_red())
                            .hover(|s| s.bg(Theme::tint_red_border()).text_color(Theme::text_primary()))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.write_to_clipboard(gpui::ClipboardItem::new_string(msg_for_copy.clone()));
                                this.notice = Some("已成功复制报错详情至剪贴板".to_string());
                                cx.notify();
                            }))
                            .child("复制详情"),
                    )
                    .child(
                        div()
                            .id("global-error-banner-dismiss")
                            .flex_shrink_0()
                            .px(px(Theme::SPACE_2))
                            .py(px(Theme::SPACE_0_5))
                            .rounded(px(Theme::RADIUS_SM))
                            .cursor_pointer()
                            .text_size(px(Theme::TEXT_SMALL))
                            .text_color(Theme::accent_red())
                            .hover(|s| s.bg(Theme::tint_red_border()))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.state.status = ProcessStatus::Idle;
                                cx.notify();
                            }))
                            .child("知道了"),
                    ),
            )
            .into_any_element()
    }

    /// 一次性中性提示条（成功类反馈）。
    ///
    /// 与 [`Self::render_error_banner`] 分开：那条是「出错了」（红），这条是
    /// 「操作完成了，但结果需要告知」（中性色）。典型场景是视频库删掉记录后
    /// 说明「当前工程被换成了哪一条」——操作本身成功，不该报成错误。
    pub(crate) fn render_notice_banner(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(msg) = self.notice.clone() else {
            return div().into_any_element();
        };

        div()
            .id("global-notice-banner")
            .w_full()
            .flex_shrink_0()
            .px(px(Theme::PAGE_PAD))
            .py(px(Theme::SPACE_2))
            .bg(Theme::bg_raised())
            .border_b_1()
            .border_color(Theme::border_mid())
            .flex()
            .items_center()
            .gap(px(Theme::SPACE_2))
            .child(primitives::stat_dot_sm(Theme::accent_mint()))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .text_size(px(Theme::TEXT_BODY))
                    .text_color(Theme::text_secondary())
                    .child(msg),
            )
            .child(
                div()
                    .id("global-notice-banner-dismiss")
                    .flex_shrink_0()
                    .px(px(Theme::SPACE_2))
                    .py(px(Theme::SPACE_0_5))
                    .rounded(px(Theme::RADIUS_SM))
                    .cursor_pointer()
                    .text_size(px(Theme::TEXT_SMALL))
                    .text_color(Theme::text_muted())
                    .hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::text_primary()))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.notice = None;
                        cx.notify();
                    }))
                    .child("知道了"),
            )
            .into_any_element()
    }

    /// 上次运行崩溃的提示条（可关闭，且带「导出诊断」入口）。
    ///
    /// # 为什么单独一条、不复用 `notice`
    ///
    /// `notice` 是「刚刚做了一件事」的一次性反馈，会被下一个操作覆盖掉；崩溃提示必须
    /// **一直挂着**直到用户明确关掉，否则启动后随手点个按钮就再也看不见了，而这个
    /// 提示的全部价值就在于让用户知道「上次崩过、日志在哪、怎么反馈」。
    ///
    /// 配色用琥珀（与术语违规、provider 回落同源）：崩溃是「需要注意」但不是「当前
    /// 出错」——程序此刻是好的，用户不必紧张。
    pub(crate) fn render_crash_banner(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(msg) = self.crash_notice.clone() else {
            return div().into_any_element();
        };
        div()
            .id("crash-notice-banner")
            .w_full()
            .flex_shrink_0()
            .px(px(Theme::PAGE_PAD))
            .py(px(Theme::SPACE_2))
            .bg(Theme::tint_warn_soft())
            .border_b_1()
            .border_color(Theme::tint_warn_border())
            .flex()
            .items_center()
            .gap(px(Theme::SPACE_2))
            .child(primitives::stat_dot_sm(Theme::accent_orange()))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .text_size(px(Theme::TEXT_BODY))
                    .text_color(Theme::text_secondary())
                    .child(msg),
            )
            .child(
                // 「复制报告」要放在「导出诊断」前面：崩溃时用户最需要的是**堆栈**，
                // 而导出的是环境诊断（不含日志正文），两者不是一件事。
                primitives::chip_clickable("复制报告", false, false)
                    .id("crash-banner-copy")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.copy_crash_report(cx);
                    })),
            )
            .child(
                primitives::chip_clickable("导出诊断", false, false)
                    .id("crash-banner-export")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.export_diagnostics(cx);
                    })),
            )
            .child(
                div()
                    .id("crash-banner-dismiss")
                    .flex_shrink_0()
                    .px(px(Theme::SPACE_2))
                    .py(px(Theme::SPACE_0_5))
                    .rounded(px(Theme::RADIUS_SM))
                    .cursor_pointer()
                    .text_size(px(Theme::TEXT_SMALL))
                    .text_color(Theme::text_muted())
                    .hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::text_primary()))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.crash_notice = None;
                        cx.notify();
                    }))
                    .child("知道了"),
            )
            .into_any_element()
    }

    /// 渲染底部状态与操作栏 (iOS Minimal Toolbar 规范)
    #[allow(dead_code)]
    pub(crate) fn render_bottom_timeline(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let is_processing = matches!(self.state.status, ProcessStatus::Processing { .. });
        let has_file = self.state.transcribe_file.is_some();
        let has_batch = !self.state.batch_queue.is_empty();
        let can_start = (has_file || has_batch) && !is_processing;
        let can_export = !self.state.segments.is_empty();
        let can_play = self.state.selected_file.is_some() && can_export && !is_processing;

        // 失败态：状态栏必须说清「为什么失败」，而不是只丢一个「出错」——用户在语音转写页
        // 看到红点却分不出是转写失败、导出失败还是设置没存上。取原文后，摘要（截断到一行）
        // 与悬停浮层（看全文）都从同一条 `Failed(msg)` 派生，口径不会走偏。
        let failure: Option<(String, String)> = match &self.state.status {
            ProcessStatus::Failed(msg) => {
                // 生产者没给原因时也要有可读兜底，状态栏不能空着
                let full = if msg.trim().is_empty() {
                    FAILURE_SUMMARY_PLACEHOLDER.to_string()
                } else {
                    msg.clone()
                };
                Some((failure_summary(msg), full))
            }
            _ => None,
        };

        // 只取阶段名与进度即可：`detail` 从未被消费（下游只用 stage/progress），
        // 而它是 `String`——每帧 `clone()` 一个可能非空的详情串纯属浪费。改为只借用。
        let (stage_text, _progress_val) = match &self.state.status {
            ProcessStatus::Idle => ("就绪", 0.0),
            ProcessStatus::Processing {
                stage, progress, ..
            } => (stage.as_str(), *progress),
            ProcessStatus::Completed => ("转写完成", 1.0),
            // 失败态的可见文案取自上面的 `failure`（截断摘要），不从这里出；
            // `progress_val` 只在进行中分支被消费，这里给 0 不影响任何渲染。
            ProcessStatus::Failed(_) => ("", 0.0),
        };

        div()
            .w_full()
            .h(px(Theme::STATUS_BAR_H))
            .flex_shrink_0()
            .bg(Theme::bg_sidebar())
            .border_t_1()
            .border_color(Theme::border())
            .px(px(Theme::PAGE_PAD))
            .flex()
            .items_center()
            .justify_between()
            // 左侧状态指示
            // 左区允许收缩（`flex_1` + `min_w(0)`）：失败摘要过长时在文本上截断，
            // 而不是把右侧操作按钮挤出栏外。
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(Theme::SPACE_3))
                    .flex_1()
                    .min_w(px(0.0))
                    .child(if is_processing {
                        let total_dur = self.state.transcribe_duration;
                        let cur_sec = self.state.streaming_current_sec;
                        // `mm:ss` 文本统一走 `ui::format_mm_ss`：异常时长（负数 / NaN / 无穷）
                        // 回退成 00:00，渲染路径上不再做 `as u32` 截断。
                        let display_ratio = if total_dur > 0.0 && cur_sec > 0.0 {
                            (cur_sec / total_dur).clamp(0.0, 1.0)
                        } else {
                            0.0
                        };

                        div()
                            .flex()
                            .flex_col()
                            .gap(px(Theme::SPACE_1))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(Theme::SPACE_2))
                                    .child(
                                        div()
                                            .text_size(px(Theme::TEXT_BODY))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(Theme::text_primary())
                                            .child(stage_text.to_string()),
                                    )
                                    .child(if total_dur > 0.0 {
                                        div()
                                            .text_size(px(Theme::TEXT_SMALL))
                                            .text_color(Theme::text_secondary())
                                            .child(format!(
                                                "{} / {}",
                                                crate::ui::format_mm_ss(cur_sec),
                                                crate::ui::format_mm_ss(total_dur)
                                            ))
                                    } else {
                                        div()
                                    })
                                    .child(primitives::badge_accent(format!(
                                        "{:.1}%",
                                        display_ratio * 100.0
                                    ))),
                            )
                            .child(
                                div()
                                    .w(px(Theme::PROGRESS_W))
                                    .child(primitives::progress_bar(display_ratio as f32)),
                            )
                    } else if let Some((summary, full)) = failure {
                        div()
                            .flex()
                            .items_center()
                            .gap(px(Theme::SPACE_2))
                            .min_w(px(0.0))
                            .child(primitives::stat_dot_sm(Theme::accent_red()))
                            .child(
                                div()
                                    .id("status-bar-failure")
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .truncate()
                                    .text_size(px(Theme::TEXT_BODY))
                                    .text_color(Theme::accent_red())
                                    .child(summary)
                                    // 摘要被截断过，完整原因留给悬停——用户排查问题要看全
                                    .tooltip(move |_window, cx| {
                                        cx.new(|_| FailureTooltip { text: full.clone() }).into()
                                    }),
                            )
                    } else if self.state.transcribe_file.is_some() {
                        div()
                            .flex()
                            .items_center()
                            .gap(px(Theme::SPACE_2))
                            .child(primitives::stat_dot_sm(Theme::accent_blue()))
                            .child(
                                div()
                                    .text_size(px(Theme::TEXT_BODY))
                                    .text_color(Theme::text_secondary())
                                    .child("文件已就绪"),
                            )
                    } else if has_batch {
                        div()
                            .flex()
                            .items_center()
                            .gap(px(Theme::SPACE_2))
                            .child(primitives::stat_dot_sm(Theme::accent_mint()))
                            .child(
                                div()
                                    .text_size(px(Theme::TEXT_BODY))
                                    .text_color(Theme::text_secondary())
                                    .child(format!(
                                        "批量队列已就绪 · {} 个待处理",
                                        self.state.queue_pending_count()
                                    )),
                            )
                    } else {
                        div()
                            .flex()
                            .items_center()
                            .gap(px(Theme::SPACE_2))
                            .child(primitives::stat_dot_sm(Theme::bg_dot_idle()))
                            .child(
                                div()
                                    .text_size(px(Theme::TEXT_BODY))
                                    .text_color(Theme::text_muted())
                                    .child("就绪"),
                            )
                    }),
            )
            // 右侧核心操作按钮 (iOS 椭圆胶囊 Pill Buttons)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(Theme::SPACE_2_5))
                    .flex_shrink_0()
                    .child(
                        // 「播放预览」：禁用态只置灰不接交互，形状不变
                        primitives::pill_btn_outline_state("播放预览", can_play)
                            .id("play-video-btn")
                            .when(can_play, |d| {
                                d.on_click(cx.listener(|this, _, _, cx| this.play_video(cx)))
                            }),
                    )
                    .child(
                        // 「开始转写 / 开始全部」：实心薄荷胶囊，禁用时退化为中性底槽
                        primitives::pill_btn_solid_state(
                            start_button_label(
                                is_processing,
                                self.state.batch_running,
                                has_file,
                                has_batch,
                            ),
                            Theme::accent_mint(),
                            can_start,
                        )
                        .id("start-pipeline-btn")
                        .when(can_start, |d| {
                            d.on_click(cx.listener(|this, _, _, cx| {
                                if this.state.transcribe_file.is_some() {
                                    this.start_processing(cx);
                                } else {
                                    this.start_batch_queue(cx);
                                }
                            }))
                        }),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{
        failure_summary, start_button_label, FAILURE_SUMMARY_MAX_CHARS, FAILURE_SUMMARY_PLACEHOLDER,
    };

    /// 归位：同一颗按钮在四种状态下必须说清「点下去会做什么」，且用词与转写配置
    /// 抽屉的主 CTA 同源（都说「转写」）——同一件事不该在底部工具栏里换个说法。
    #[test]
    fn start_button_label_matches_drawer_cta() {
        assert_eq!(start_button_label(false, false, true, false), "开始转写");
        assert_eq!(start_button_label(false, false, false, true), "开始全部");
        // 无文件也无队列：按钮是禁用态，文案仍要预告补上文件后的动作
        assert_eq!(start_button_label(false, false, false, false), "开始转写");
        assert_eq!(start_button_label(true, false, true, false), "转写中…");
        // 批量进行中：必须能看出跑的是整批，而不是当前这一条
        assert_eq!(start_button_label(true, true, true, true), "批量转写中…");
    }

    /// 进行中态优先于文件/队列判定：任何组合都不许漏出「开始…」文案，否则用户会
    /// 在转写途中看到一颗写着「开始转写」的按钮，以为任务没跑起来。
    #[test]
    fn running_states_never_show_start_wording() {
        for (batch_running, has_file, has_batch) in [
            (false, true, false),
            (false, false, true),
            (true, true, true),
        ] {
            let label = start_button_label(true, batch_running, has_file, has_batch);
            assert!(
                label.contains("转写"),
                "进行中文案 {label} 没说明自己在转写"
            );
            assert!(
                !label.starts_with("开始"),
                "进行中文案 {label} 不该出现在「开始…」这一档"
            );
            assert!(
                label.ends_with('…'),
                "进行中文案 {label} 用省略号表示还在跑"
            );
        }
    }

    /// 短文本原样返回：摘要只在放不下时才动手，不该顺手改写正常文案。
    #[test]
    fn failure_summary_keeps_short_text() {
        let short = "转写失败：模型文件缺失";
        assert_eq!(failure_summary(short), short);
    }

    /// 空串 / 纯空白给占位文案：生产者没给原因时状态栏不能空着，空白会被读成「没出错」。
    #[test]
    fn failure_summary_falls_back_to_placeholder() {
        assert_eq!(failure_summary(""), FAILURE_SUMMARY_PLACEHOLDER);
        assert_eq!(failure_summary("   \n\t "), FAILURE_SUMMARY_PLACEHOLDER);
    }

    /// 换行与连续空白折叠成单个空格，并去掉首尾空白：状态栏只有一行高，
    /// 原文里的换行会把栏位撑高、多空格会把摘要往右推。
    #[test]
    fn failure_summary_flattens_whitespace_to_one_line() {
        assert_eq!(
            failure_summary("导出失败\n\n  磁盘已满"),
            "导出失败 磁盘已满"
        );
        assert_eq!(failure_summary("  转写失败  "), "转写失败");
        assert!(!failure_summary("a\nb").contains('\n'));
    }

    /// 超长文本按**字符**边界截断并补「…」：字符数恰好是上限 +1（省略号），
    /// 且保留部分与上限字数逐字相同。
    #[test]
    fn failure_summary_truncates_long_text_at_char_boundary() {
        let long = "错".repeat(100);
        let out = failure_summary(&long);
        assert_eq!(out.chars().count(), FAILURE_SUMMARY_MAX_CHARS + 1);
        assert!(out.ends_with('…'));
        assert_eq!(out, format!("{}…", "错".repeat(FAILURE_SUMMARY_MAX_CHARS)));
    }

    /// 多字节安全：截断点正好落在 4 字节 emoji 之后也不许劈坏码位——
    /// 结果是合法 UTF-8，保留的最后一个字符是完整 emoji 而不是半个字节。
    #[test]
    fn failure_summary_is_multibyte_safe() {
        // 共 MAX+1 个字符，截到 MAX 个时尾巴正好是一个完整 emoji
        let long = format!("{}🎬🎬", "a".repeat(FAILURE_SUMMARY_MAX_CHARS - 1));
        let out = failure_summary(&long);
        assert_eq!(out.chars().count(), FAILURE_SUMMARY_MAX_CHARS + 1);
        assert_eq!(
            out,
            format!("{}🎬…", "a".repeat(FAILURE_SUMMARY_MAX_CHARS - 1))
        );
        // 结果本身是合法 UTF-8：每个字节都能被 String 重新解析
        assert_eq!(String::from_utf8(out.clone().into_bytes()), Ok(out));
    }
}
