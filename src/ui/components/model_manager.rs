//! 模型与外部组件管理面板（首次使用引导）。
//!
//! # 为什么要有它
//!
//! 模型与可执行文件合计约 2.9 GB，不进版本库。此前用户 clone 下来只拿到源码，
//! 启动后各引擎报「未就绪」，却没有任何界面告诉他缺什么、去哪拿。
//!
//! # 下载源
//!
//! 全部走 `hf-mirror.com`（HuggingFace 国内镜像），**不需要梯子**。
//! 2026-10-05 在本机（无代理）实测：`github.com` 的 release 附件超时不可达，
//! 而 hf-mirror 上全部条目均可下（实测下载 141 MB 用时 19.7 秒）。
//! 详见 `src/utils/model_download.rs` 的模块文档。

use gpui::prelude::*;
use gpui::{div, px, AnyElement, FontWeight, IntoElement, ParentElement, Styled};

use crate::ui::primitives;
use crate::ui::theme::Theme;
use crate::ui::MainWindow;
use crate::utils::{ItemGroup, ITEMS};

/// 人类可读的体积：`141 MB` / `885 KB`。
fn human_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.0} MB", b / MB)
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

impl MainWindow {
    /// 模型管理卡片。`compact` 为真时只显示缺失项与一个「一键补齐」按钮
    /// （用于侧栏），否则显示完整清单（用于独立页面）。
    pub(crate) fn render_model_manager(
        &mut self,
        compact: bool,
        cx: &mut gpui::Context<Self>,
    ) -> AnyElement {
        let is_downloading = self.state.is_downloading;
        let done = self.state.download_done_bytes;
        let total = self.state.download_total_bytes;
        let status = self.state.download_status_msg.clone();
        let current = self.state.download_current.clone();
        let missing = self.state.missing_model_count();
        let missing_required = self.state.missing_required_models();

        // 进度比例：服务端没给 Content-Length 时退化为「不确定」态（用 0 表示）
        let ratio = if total > 0 {
            (done as f32 / total as f32).clamp(0.0, 1.0)
        } else {
            0.0
        };

        // ── 顶部：标题 + 缺失计数 ──
        let header = div().flex().items_center().justify_between().child(
            div()
                .flex()
                .items_center()
                .gap(px(Theme::SPACE_2))
                .child(primitives::section_title("模型与组件"))
                .child(if missing == 0 {
                    primitives::badge("已就绪")
                } else if missing_required > 0 {
                    primitives::badge_danger(format!("缺 {missing_required} 个必需"))
                } else {
                    primitives::badge(format!("缺 {missing} 个可选"))
                }),
        );

        // ── 说明行：明确告知「不需要梯子」──
        let note = div()
            .text_size(px(Theme::TEXT_CAPTION))
            .text_color(Theme::text_muted())
            .child(if is_downloading {
                "下载中… 可随时取消，已下载部分不会留下损坏文件"
            } else if missing == 0 {
                "全部组件就位，无需下载"
            } else {
                "下载源为国内镜像（hf-mirror），无需代理即可下载"
            });

        // ── 一键补齐 / 取消 ──
        let bulk_row = {
            let mut row = div().flex().items_center().gap(px(Theme::SPACE_2));
            if is_downloading {
                row = row.child(
                    primitives::btn_clickable(
                        "取消下载",
                        primitives::BtnSize::Md,
                        primitives::BtnVariant::Secondary,
                    )
                    .id("model-dl-cancel")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.model_download_cancel
                            .store(true, std::sync::atomic::Ordering::SeqCst);
                        this.state.download_status_msg = "正在取消下载…".to_string();
                        cx.notify();
                    })),
                );
            } else if missing > 0 {
                row = row.child(
                    primitives::btn_clickable(
                        format!("一键补齐缺失的 {missing} 个组件"),
                        primitives::BtnSize::Md,
                        primitives::BtnVariant::Primary,
                    )
                    .id("model-dl-all")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.download_all_missing(cx);
                    })),
                );
            }
            row
        };

        // ── 进度条（仅下载中出现）──
        let progress = if is_downloading {
            let label = if total > 0 {
                format!(
                    "{}  {} / {}",
                    current.as_deref().unwrap_or("准备中"),
                    human_size(done),
                    human_size(total)
                )
            } else {
                format!(
                    "{}  {}",
                    current.as_deref().unwrap_or("准备中"),
                    human_size(done)
                )
            };
            Some(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap(px(Theme::SPACE_1))
                    .child(
                        div()
                            .text_size(px(Theme::TEXT_CAPTION))
                            .text_color(Theme::text_secondary())
                            .child(label),
                    )
                    .child(if total > 0 {
                        primitives::progress_bar(ratio)
                    } else {
                        // 服务端未给长度：画一条不确定态的满宽细线，
                        // 而不是留空（留空看起来像卡死了）
                        div()
                            .w_full()
                            .h(px(Theme::PROGRESS_H))
                            .rounded_full()
                            .bg(Theme::bg_inset())
                            .border_1()
                            .border_color(Theme::border())
                    }),
            )
        } else {
            None
        };

        // ── 条目列表 ──
        let mut groups: Vec<(ItemGroup, Vec<&'static crate::utils::DownloadItem>)> = Vec::new();
        for g in [ItemGroup::Asr, ItemGroup::Text, ItemGroup::Binary] {
            let items: Vec<_> = ITEMS
                .iter()
                .filter(|i| i.group == g && (!compact || !self.state.model_is_present(i.id)))
                .collect();
            if !items.is_empty() {
                groups.push((g, items));
            }
        }

        let mut list = div().flex().flex_col().gap(px(Theme::SPACE_2));
        for (group, items) in groups {
            let mut col = div().flex().flex_col().gap(px(Theme::SPACE_1_5));
            col = col.child(
                div()
                    .text_size(px(Theme::TEXT_CAPTION))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(Theme::text_muted())
                    .child(group.label()),
            );
            for item in items {
                col = col.child(self.render_model_row(item, is_downloading, cx));
            }
            list = list.child(col);
        }

        // ── 组装 ──
        let mut card = primitives::card_sm()
            .gap(px(Theme::SPACE_2))
            .child(header)
            .child(note);
        if let Some(p) = progress {
            card = card.child(p);
        }
        card = card.child(bulk_row).child(list);
        if !status.is_empty() {
            card = card.child(
                div()
                    .text_size(px(Theme::TEXT_CAPTION))
                    .text_color(if status.contains("失败") {
                        Theme::accent_red()
                    } else if status.contains("完成") {
                        Theme::accent_mint()
                    } else {
                        Theme::text_muted()
                    })
                    .child(status),
            );
        }
        card.into_any_element()
    }

    /// 单个条目行：名称 + 说明 + 体积 + 状态徽标 / 下载按钮。
    fn render_model_row(
        &mut self,
        item: &'static crate::utils::DownloadItem,
        is_downloading: bool,
        cx: &mut gpui::Context<Self>,
    ) -> AnyElement {
        let present = self.state.model_is_present(item.id);
        // 用户自编译的自包含构建（如手编 Vulkan whisper-cli）：单独打标，
        // 且不给「下载」按钮——下载会被 download_one 拒绝（保护用户构建），
        // 与其点一下弹错，不如直接说明这是你自己的版本、无需下载。
        let custom = present && self.state.model_is_custom_build(item.id);
        let is_current = self
            .state
            .download_current
            .as_deref()
            .map(|c| c == item.id)
            .unwrap_or(false);
        let row_id = item.id;

        let status_el: AnyElement = if custom {
            primitives::badge_accent("已就位 · 自编译").into_any_element()
        } else if present {
            primitives::badge("已就位").into_any_element()
        } else if is_current && is_downloading {
            primitives::badge_accent("下载中").into_any_element()
        } else {
            primitives::btn_clickable(
                "下载",
                primitives::BtnSize::Sm,
                primitives::BtnVariant::Secondary,
            )
            .id(row_id)
            .when(!is_downloading, |s| {
                s.on_click(cx.listener(move |this, _, _, cx| {
                    this.start_model_download(row_id, cx);
                }))
            })
            .into_any_element()
        };

        div()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(Theme::SPACE_2))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(Theme::SPACE_0_5))
                    .flex_1()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(Theme::SPACE_1_5))
                            .child(
                                div()
                                    .text_size(px(Theme::TEXT_SMALL))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(Theme::text_primary())
                                    .child(item.label),
                            )
                            .child(
                                div()
                                    .text_size(px(Theme::TEXT_CAPTION))
                                    .text_color(Theme::text_muted())
                                    .child(human_size(item.size)),
                            )
                            .when(item.required, |d| d.child(primitives::badge_danger("必需"))),
                    )
                    .child(
                        div()
                            .text_size(px(Theme::TEXT_CAPTION))
                            .text_color(Theme::text_muted())
                            .child(item.note),
                    ),
            )
            .child(status_el)
            .into_any_element()
    }
}
