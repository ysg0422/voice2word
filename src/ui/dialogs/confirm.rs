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

use gpui::prelude::*;
use gpui::*;

use super::super::primitives;
use super::super::theme::Theme;
use super::super::types::{ConfirmAction, ConfirmDialogInfo};
use super::super::MainWindow;

impl MainWindow {
    /// 渲染通用确认弹窗。
    pub(crate) fn render_confirm_dialog(
        &mut self,
        info: ConfirmDialogInfo,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let action = info.action.clone();
        primitives::modal_scrim()
            .id("confirm-dialog-backdrop")
            // 点遮罩 = 取消（与点「取消」同义，符合直觉）
            .on_click(cx.listener(|this, _, _, cx| {
                this.confirm_dialog = None;
                cx.notify();
            }))
            .child(
                primitives::modal_card(Theme::DIALOG_W, Theme::PAGE_PAD)
                    .id("confirm-dialog-card")
                    // 面板自身吞掉点击，否则点正文也会被遮罩当作「取消」
                    .on_click(cx.listener(|_, _, _, _cx| {}))
                    .gap(px(Theme::SPACE_3))
                    .child(primitives::page_title(info.title.clone()))
                    .child(
                        div()
                            .text_size(px(Theme::TEXT_BODY_LG))
                            .text_color(Theme::text_secondary())
                            .child(info.message.clone()),
                    )
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
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.confirm_dialog = None;
                                    cx.notify();
                                })),
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
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.confirm_dialog = None;
                                        this.run_confirm_action(action.clone(), cx);
                                    }))
                            }),
                    ),
            )
    }

    /// 执行确认后的动作。与会话状态一起收口，避免 action 的执行逻辑散落在两个渲染文件里。
    pub(crate) fn run_confirm_action(&mut self, action: ConfirmAction, cx: &mut Context<Self>) {
        match action {
            ConfirmAction::ClearBatchQueue => self.clear_batch_queue(cx),
            ConfirmAction::DeleteTaskRecord(id) => self.delete_task_record(id, cx),
        }
        cx.notify();
    }
}
