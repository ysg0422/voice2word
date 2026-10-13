//! 通用二次确认弹窗
//!
//! # 为什么单独做一个通用弹窗
//!
//! 「清空批量队列」与「删除视频库记录」此前都是**一点即执行、不可逆**，而前者还会
//! 顺带强杀正在跑的转写。这类操作的代价远大于误触成本，必须有一次确认。
//!
//! 与其给每个调用点各写一个弹窗，不如把「标题 + 正文 + 确认键」这套外壳收敛到
//! [`ConfirmDialogInfo`]（见 `types.rs`），确认后要执行什么用 [`ConfirmAction`]
//! 编码——枚举可 `Clone + PartialEq`，能存进 `MainWindow` 状态，而闭包不能。
//!
//! # 键盘
//!
//! 回车 = 点「确认」，Tab / Shift+Tab 在「取消」「确认」之间挪焦点（见 [`keyboard`]）。
//! Esc **不在本文件处理**：`ui/mod.rs` 的 `CancelOrClose` 优先级链第一刀就是
//! `this.confirm_dialog.take()`（链里「确认弹窗排在最前」那段注释即为此），本框
//! 已被它覆盖，这里再挂一个 Esc 分支只会让同一个键跑两遍。

use gpui::prelude::*;
use gpui::*;

use super::super::primitives;
use super::super::theme::Theme;
use super::super::types::{ConfirmAction, ConfirmDialogInfo};
use super::super::MainWindow;
use super::keyboard;

/// 把确认框正文里的 `**强调**` 标记拆成「纯文本 + 强调区间」。
///
/// 返回的区间是**字节偏移**（`StyledText::with_highlights` 的入参要求），只覆盖
/// `**` 包住的内容本身；标记符不进入纯文本，所以界面上不会再出现裸星号。
///
/// 为什么要解析而不是让调用点去掉星号：调用点写 `**` 是想表达「这一段是后果
/// 最重的一句」，直接删标记会把语气一并删掉；把标记收进渲染层后，调用点继续用
/// 自然写法，以后新增的确认框也自动获得强调能力。
///
/// 未闭合的 `**` 原样保留、不吞后续文字：宁可多显示两个星号，也不要把用户没
/// 写完的正文吃掉。
fn split_emphasis(message: &str) -> (String, Vec<std::ops::Range<usize>>) {
    let mut plain = String::with_capacity(message.len());
    let mut emphasis = Vec::new();
    let mut rest = message;
    while let Some(open) = rest.find("**") {
        let after_open = &rest[open + 2..];
        // 找不到配对的收尾标记：跳出循环，剩余部分按字面保留（见函数末尾 push）。
        let Some(close) = after_open.find("**") else {
            break;
        };
        plain.push_str(&rest[..open]);
        let start = plain.len();
        plain.push_str(&after_open[..close]);
        let end = plain.len();
        // 空的 `****` 不产生区间（否则 highlight 会退化成一个零宽 run）。
        if end > start {
            emphasis.push(start..end);
        }
        rest = &after_open[close + 2..];
    }
    plain.push_str(rest);
    (plain, emphasis)
}

impl MainWindow {
    /// 渲染通用确认弹窗。
    pub(crate) fn render_confirm_dialog(
        &mut self,
        info: ConfirmDialogInfo,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        // 键盘支持：注册全局按键拦截（幂等，只有第一次真的干活），再取回本框的
        // 焦点句柄（`thread_local` 里跨帧复用）并挂到卡片和两个按钮上。
        keyboard::ensure_handler(cx);
        let panel_focus = keyboard::panel_focus(keyboard::DialogKind::Confirm, cx);
        let cancel_focus = keyboard::button_focus(
            keyboard::DialogKind::Confirm,
            0,
            keyboard::DialogAction::ConfirmCancel,
            cx,
        );
        let accept_focus = keyboard::button_focus(
            keyboard::DialogKind::Confirm,
            1,
            keyboard::DialogAction::ConfirmAccept,
            cx,
        );

        primitives::modal_scrim()
            .id("confirm-dialog-backdrop")
            // 点遮罩 = 取消（与点「取消」同义，符合直觉）
            .on_click(cx.listener(|this, _, _, cx| this.cancel_confirm_dialog(cx)))
            .child(
                primitives::modal_card(Theme::DIALOG_W, Theme::PAGE_PAD)
                    .id("confirm-dialog-card")
                    // 卡片是这个弹窗的焦点容器：Tab 进来先落到它，再按 GPUI 的 Tab
                    // 顺序走到两个按钮上（按钮句柄带 `tab_stop`）。
                    .track_focus(&panel_focus)
                    // 面板自身吞掉点击，否则点正文也会被遮罩当作「取消」。
                    //
                    // 光挂一个空的 on_click 是不够的：GPUI 的点击沿命中链冒泡，
                    // 遮罩与卡片都命中时两者都会收到 click，空回调并不会中断冒泡，
                    // 遮罩的「点空白 = 取消」照样触发。必须让卡片 `occlude()`，把
                    // 遮罩从命中集合里挡掉，点正文/按钮才不会误判成取消。
                    .occlude()
                    .on_click(cx.listener(|_, _, _, _cx| {}))
                    .gap(px(Theme::SPACE_3))
                    .child(primitives::page_title(info.title.clone()))
                    .child({
                        // 正文支持 `**强调**` 片段：拆出强调区间后交给 StyledText 单独
                        // 上色/加粗，其余保持次级色。用单条 StyledText（而不是拼多个 div）
                        // 是为了保留整段文字的正常换行——拆成多个 flex 子节点后长句就不会
                        // 在强调边界之外断行了。
                        let (plain, emphasis) = split_emphasis(&info.message);
                        let highlights = emphasis.into_iter().map(|range| {
                            (
                                range,
                                HighlightStyle {
                                    // 强调段用主文字色 + 加粗，在次级色正文里拉开层级
                                    color: Some(Theme::text_primary().into()),
                                    font_weight: Some(FontWeight::BOLD),
                                    ..Default::default()
                                },
                            )
                        });
                        div()
                            .text_size(px(Theme::TEXT_BODY_LG))
                            .text_color(Theme::text_secondary())
                            .child(StyledText::new(plain).with_highlights(highlights))
                    })
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap(px(Theme::SPACE_2))
                            .child(
                                primitives::btn_clickable(
                                    "取消",
                                    primitives::BtnSize::Md,
                                    primitives::BtnVariant::Secondary,
                                )
                                .id("confirm-dialog-cancel")
                                .track_focus(&cancel_focus)
                                .focus(|s| s.border_1().border_color(Theme::accent_blue()))
                                .on_click(cx.listener(
                                    |this, _, _, cx| {
                                        this.cancel_confirm_dialog(cx);
                                    },
                                )),
                            )
                            .child({
                                let label = info.confirm_label.clone();
                                let variant = if info.danger {
                                    primitives::BtnVariant::Danger
                                } else {
                                    primitives::BtnVariant::Primary
                                };
                                primitives::btn_clickable(label, primitives::BtnSize::Md, variant)
                                    .id("confirm-dialog-accept")
                                    .track_focus(&accept_focus)
                                    .focus(|s| s.border_1().border_color(Theme::accent_blue()))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.accept_confirm_dialog(cx);
                                    }))
                            }),
                    ),
            )
    }

    /// 执行「确认」。
    ///
    /// 鼠标点「确认」按钮与键盘按回车都调它，所以两条路径永远一致：动作从**当前**
    /// 弹窗状态里取，不在这里另存一份。
    pub(crate) fn accept_confirm_dialog(&mut self, cx: &mut Context<Self>) {
        let Some(info) = self.confirm_dialog.take() else {
            return;
        };
        self.run_confirm_action(info.action.clone(), cx);
    }

    /// 执行「取消」（点遮罩、点「取消」按钮共用）。
    pub(crate) fn cancel_confirm_dialog(&mut self, cx: &mut Context<Self>) {
        if self.confirm_dialog.take().is_some() {
            cx.notify();
        }
    }

    /// 执行确认后的动作。与会话状态一起收口，避免 action 的执行逻辑散落在两个渲染文件里。
    pub(crate) fn run_confirm_action(&mut self, action: ConfirmAction, cx: &mut Context<Self>) {
        match action {
            ConfirmAction::ClearBatchQueue => self.clear_batch_queue(cx),
            ConfirmAction::DeleteTaskRecord(id) => self.delete_task_record(id, cx),
            ConfirmAction::DeleteModelFile(id) => self.delete_model_file(&id, cx),
            ConfirmAction::RestoreBackup(path) => self.restore_data_backup(&path, cx),
        }
        cx.notify();
    }
}
