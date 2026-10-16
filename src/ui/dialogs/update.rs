//! 软件更新详情与一键升级弹窗
//!
//! 提供版本号对比、更新日志展示、国内镜像实时下载进度条、以及下载完成后的一键安装升级。

use gpui::prelude::*;
use gpui::*;

use super::super::primitives;
use super::super::theme::Theme;
use super::super::MainWindow;

impl MainWindow {
    /// 渲染版本更新模态弹窗
    pub(crate) fn render_update_modal(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let release = self.state.update_release_info.clone();
        let target = self.state.update_download_target.clone();
        let is_downloading = self.state.update_download_busy;
        let progress = self.state.update_download_progress;
        let status_msg = self.state.update_download_status.clone();
        let downloaded_path = self.state.update_downloaded_path.clone();

        let cur_ver = crate::utils::update_check::current_version();
        let new_ver = release
            .as_ref()
            .map(|r| r.version.as_str())
            .unwrap_or(cur_ver);
        let published_at = release
            .as_ref()
            .map(|r| r.published_at.as_str())
            .unwrap_or("");
        let raw_notes = release
            .as_ref()
            .map(|r| r.notes.trim())
            .filter(|s| !s.is_empty())
            .unwrap_or("本次更新包含性能提升与稳定性修复。");
        let notes = format_release_notes(raw_notes);

        let asset_name = target
            .as_ref()
            .map(|(_, name, _)| name.as_str())
            .unwrap_or("Voice2Word-Setup.exe");
        let asset_size_mb = target
            .as_ref()
            .map(|(_, _, size)| *size as f64 / 1024.0 / 1024.0)
            .unwrap_or(0.0);

        primitives::modal_scrim()
            .id("update-modal-scrim")
            .on_click(cx.listener(|this, _, _, cx| {
                if !this.state.update_download_busy {
                    this.update_modal_open = false;
                    cx.notify();
                }
            }))
            .child(
                primitives::modal_card(540.0, Theme::PAGE_PAD)
                    .id("update-modal-card")
                    .gap(px(Theme::SPACE_3))
                    .on_click(|_, _, _| {
                        // 阻止点击弹窗内容冒泡关闭
                    })
                    // ── 顶部标头 ──
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(Theme::SPACE_2))
                                    .child(
                                        div()
                                            .w(px(28.0))
                                            .h(px(28.0))
                                            .rounded_md()
                                            .bg(Theme::accent_mint())
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .text_size(px(Theme::TEXT_BODY))
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(Theme::text_on_accent())
                                            .child("↑"),
                                    )
                                    .child(
                                        div()
                                            .flex()
                                            .flex_col()
                                            .gap(px(2.0))
                                            .child(
                                                div()
                                                    .text_size(px(Theme::TEXT_BODY))
                                                    .font_weight(FontWeight::BOLD)
                                                    .text_color(Theme::text_primary())
                                                    .child("Voice2Word 软件更新"),
                                            )
                                            .child(
                                                div()
                                                    .text_size(px(Theme::TEXT_CAPTION))
                                                    .text_color(Theme::text_muted())
                                                    .child(if published_at.is_empty() {
                                                        "发现新版本可用".to_string()
                                                    } else {
                                                        format!("发布时间：{published_at}")
                                                    }),
                                            ),
                                    ),
                            )
                            // 右上角关闭按钮
                            .child(
                                div()
                                    .id("update-modal-close-btn")
                                    .w(px(24.0))
                                    .h(px(24.0))
                                    .rounded_sm()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .cursor_pointer()
                                    .text_size(px(Theme::TEXT_BODY))
                                    .text_color(Theme::text_muted())
                                    .hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::text_primary()))
                                    .child("✕")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.update_modal_open = false;
                                        cx.notify();
                                    })),
                            ),
                    )
                    // ── 版本对比与包体积卡片 ──
                    .child(
                        div()
                            .w_full()
                            .px(px(Theme::SPACE_3))
                            .py(px(Theme::SPACE_2_5))
                            .rounded_md()
                            .bg(Theme::bg_inset())
                            .border_1()
                            .border_color(Theme::border_light())
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(Theme::SPACE_2))
                                    .child(
                                        div()
                                            .px(px(Theme::SPACE_2))
                                            .py(px(Theme::SPACE_0_5))
                                            .rounded_sm()
                                            .bg(Theme::bg_raised())
                                            .border_1()
                                            .border_color(Theme::border_mid())
                                            .text_size(px(Theme::TEXT_SMALL))
                                            .text_color(Theme::text_muted())
                                            .child(format!("当前版本 v{cur_ver}")),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(Theme::TEXT_BODY))
                                            .text_color(Theme::accent_mint())
                                            .child("➜"),
                                    )
                                    .child(
                                        div()
                                            .px(px(Theme::SPACE_2))
                                            .py(px(Theme::SPACE_0_5))
                                            .rounded_sm()
                                            .bg(Theme::bg_inset())
                                            .border_1()
                                            .border_color(Theme::accent_mint())
                                            .text_size(px(Theme::TEXT_SMALL))
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(Theme::accent_mint())
                                            .child(format!("最新版本 v{new_ver}")),
                                    ),
                            )
                            .child(
                                div()
                                    .text_size(px(Theme::TEXT_CAPTION))
                                    .text_color(Theme::text_muted())
                                    .child(if asset_size_mb > 0.0 {
                                        format!("{asset_name} (~{:.1} MB)", asset_size_mb)
                                    } else {
                                        asset_name.to_string()
                                    }),
                            ),
                    )
                    // ── 更新日志说明区域 ──
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .flex_col()
                            .gap(px(Theme::SPACE_1_5))
                            .child(
                                div()
                                    .text_size(px(Theme::TEXT_SMALL))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(Theme::text_primary())
                                    .child("更新日志 / Release Notes"),
                            )
                            .child(
                                div()
                                    .id("update-modal-notes-scroll")
                                    .w_full()
                                    .max_h(px(160.0))
                                    .overflow_y_scroll()
                                    .p(px(Theme::SPACE_2_5))
                                    .rounded_md()
                                    .bg(Theme::bg_sidebar())
                                    .border_1()
                                    .border_color(Theme::border_light())
                                    .text_size(px(Theme::TEXT_SMALL))
                                    .text_color(Theme::text_secondary())
                                    .line_height(relative(1.4))
                                    .child(notes.to_string()),
                            ),
                    )
                    // ── 下载进度与状态条（下载中或下载完成时展示） ──
                    .children(if is_downloading || downloaded_path.is_some() || progress > 0.0 {
                        Some(
                            div()
                                .w_full()
                                .flex()
                                .flex_col()
                                .gap(px(Theme::SPACE_1))
                                .child(
                                    div()
                                        .flex()
                                        .justify_between()
                                        .text_size(px(Theme::TEXT_CAPTION))
                                        .text_color(Theme::text_muted())
                                        .child(if let Some(msg) = &status_msg {
                                            msg.clone()
                                        } else if downloaded_path.is_some() {
                                            "更新包下载完成，准备就绪".to_string()
                                        } else {
                                            "正在下载更新...".to_string()
                                        })
                                        .child(format!("{:.0}%", progress * 100.0)),
                                )
                                .child(
                                    div()
                                        .w_full()
                                        .h(px(6.0))
                                        .rounded_full()
                                        .bg(Theme::bg_raised())
                                        .child(
                                            div()
                                                .h_full()
                                                .rounded_full()
                                                .bg(if downloaded_path.is_some() {
                                                    Theme::accent_mint()
                                                } else {
                                                    Theme::accent_blue()
                                                })
                                                .w(relative(progress.clamp(0.01, 1.0))),
                                        ),
                                ),
                        )
                    } else {
                        None
                    })
                    // ── 底部操作按钮栏 ──
                    .child(
                        div()
                            .w_full()
                            .pt(px(Theme::SPACE_2))
                            .border_t_1()
                            .border_color(Theme::border_light())
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .id("update-modal-open-page-btn")
                                    .px(px(Theme::SPACE_2_5))
                                    .py(px(Theme::SPACE_1_5))
                                    .rounded_md()
                                    .cursor_pointer()
                                    .text_size(px(Theme::TEXT_SMALL))
                                    .text_color(Theme::text_muted())
                                    .hover(|s| s.text_color(Theme::text_primary()))
                                    .child("浏览器中打开 ↗")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.open_release_page(cx);
                                    })),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(Theme::SPACE_2))
                                    .children(if let Some(_path) = &downloaded_path {
                                        vec![
                                            div()
                                                .id("update-modal-open-folder-btn")
                                                .px(px(Theme::SPACE_3))
                                                .py(px(Theme::SPACE_1_5))
                                                .rounded_md()
                                                .bg(Theme::bg_raised())
                                                .border_1()
                                                .border_color(Theme::border_mid())
                                                .text_size(px(Theme::TEXT_SMALL))
                                                .text_color(Theme::text_primary())
                                                .cursor_pointer()
                                                .hover(|s| s.bg(Theme::bg_hover()))
                                                .child("打开所在目录")
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.open_downloaded_update(cx);
                                                }))
                                                .into_any_element(),
                                            div()
                                                .id("update-modal-install-btn")
                                                .px(px(Theme::SPACE_4))
                                                .py(px(Theme::SPACE_1_5))
                                                .rounded_md()
                                                .bg(Theme::accent_mint())
                                                .text_size(px(Theme::TEXT_SMALL))
                                                .font_weight(FontWeight::BOLD)
                                                .text_color(Theme::text_on_accent())
                                                .cursor_pointer()
                                                .hover(|s| s.bg(Theme::accent_mint_deep()))
                                                .child("立即退出并安装升级")
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.install_downloaded_update(cx);
                                                }))
                                                .into_any_element(),
                                        ]
                                    } else if is_downloading {
                                        vec![
                                            div()
                                                .id("update-modal-cancel-btn")
                                                .px(px(Theme::SPACE_3))
                                                .py(px(Theme::SPACE_1_5))
                                                .rounded_md()
                                                .bg(Theme::bg_raised())
                                                .border_1()
                                                .border_color(Theme::border_mid())
                                                .text_size(px(Theme::TEXT_SMALL))
                                                .text_color(Theme::text_muted())
                                                .cursor_pointer()
                                                .hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::text_primary()))
                                                .child("取消下载")
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.cancel_update_download(cx);
                                                }))
                                                .into_any_element(),
                                        ]
                                    } else {
                                        vec![
                                            div()
                                                .id("update-modal-later-btn")
                                                .px(px(Theme::SPACE_3))
                                                .py(px(Theme::SPACE_1_5))
                                                .rounded_md()
                                                .bg(Theme::bg_raised())
                                                .border_1()
                                                .border_color(Theme::border_mid())
                                                .text_size(px(Theme::TEXT_SMALL))
                                                .text_color(Theme::text_muted())
                                                .cursor_pointer()
                                                .hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::text_primary()))
                                                .child("暂不更新")
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.update_modal_open = false;
                                                    cx.notify();
                                                }))
                                                .into_any_element(),
                                            div()
                                                .id("update-modal-download-btn")
                                                .px(px(Theme::SPACE_4))
                                                .py(px(Theme::SPACE_1_5))
                                                .rounded_md()
                                                .bg(Theme::accent_mint())
                                                .text_size(px(Theme::TEXT_SMALL))
                                                .font_weight(FontWeight::BOLD)
                                                .text_color(Theme::text_on_accent())
                                                .cursor_pointer()
                                                .hover(|s| s.bg(Theme::accent_mint_deep()))
                                                .child("立即下载更新")
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.download_app_update(cx);
                                                }))
                                                .into_any_element(),
                                        ]
                                    }),
                            ),
                    ),
            )
    }
}

/// 清理并美化 Release Notes，去除原生 UI 里丑陋的 Markdown 标记与表格线
fn format_release_notes(raw: &str) -> String {
    let mut lines = Vec::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if lines.last().map(|s: &String| !s.is_empty()).unwrap_or(false) {
                lines.push(String::new());
            }
            continue;
        }
        // 过滤冗余的大标题（如 ## Voice2Word v0.2.9，弹窗顶部已大字展示）
        if trimmed.starts_with("# ") || trimmed.starts_with("## ") {
            continue;
        }
        // 过滤 Markdown 表格分割线（形如 |:---|:---|）
        if trimmed.starts_with('|') && trimmed.contains("---") {
            continue;
        }
        // Markdown 表格行：转为整洁的纯文本条目
        if trimmed.starts_with('|') && trimmed.ends_with('|') {
            let cells: Vec<&str> = trimmed
                .trim_matches('|')
                .split('|')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect();
            if cells.is_empty() || cells[0] == "文件" {
                continue;
            }
            let name = cells[0].trim_matches(|c| c == '*' || c == '`');
            let desc = cells.get(2).copied().or_else(|| cells.get(1).copied()).unwrap_or("");
            let clean_desc = desc.replace("**", "").replace('`', "");
            lines.push(format!("• {name}：{clean_desc}"));
            continue;
        }

        // 普通小标题与列表：清除 ###、**、` 等原始标记
        let cleaned = trimmed
            .trim_start_matches('#')
            .trim()
            .replace("**", "")
            .replace('`', "");
        if !cleaned.is_empty() {
            lines.push(cleaned);
        }
    }
    let res = lines.join("\n").trim().to_string();
    if res.is_empty() {
        "本次更新包含性能提升与稳定性修复。".to_string()
    } else {
        res
    }
}

