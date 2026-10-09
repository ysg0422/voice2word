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

// 体积格式化统一走 `utils::model_download::human_size`（单一实现处）：
// 本文件曾有一份私有副本，与下载层各写各的规则，迟早出现「同一文件在模型卡与
// 下载进度里显示成两个数」。这里只做引入，不再重复实现。
use crate::utils::model_download::human_size;

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

        // 占用统计：只 stat 各条目**已解析到的实际路径**（十来个文件，且卡片不是
        // 逐帧重绘的重灾区），让用户一眼看到「这些模型占了我多少盘」。放在按钮行
        // 左边，与「删除」一起构成磁盘管理的最小闭环：看到占用 → 决定删哪个。
        let presence = crate::utils::model_download::PresenceContextRef::new(&self.state.config);
        let (used_bytes, used_count) = ITEMS
            .iter()
            .filter(|i| presence.is_present(i))
            .filter_map(|i| {
                // 路径解析与存在性判定同源（`item_effective_path`），不在这里
                // 自己拼「配置优先 / 默认兜底」——那份规则一旦分叉就会出现
                // 「显示已就位、统计却算到另一个文件」。
                let p = crate::utils::model_download::item_effective_path(i, &self.state.config);
                crate::utils::model_download::disk_size(&p)
            })
            .fold((0u64, 0usize), |(bytes, n), sz| (bytes + sz, n + 1));

        // ── 一键补齐 / 取消 ──
        let bulk_row = {
            let mut row = div().flex().items_center().gap(px(Theme::SPACE_2));
            if used_count > 0 {
                row = row.child(
                    div()
                        .text_size(px(Theme::TEXT_CAPTION))
                        .text_color(Theme::text_muted())
                        .child(format!(
                            "已就位 {used_count} 项 · 共占 {}",
                            human_size(used_bytes)
                        )),
                );
            }
            row = row.child(
                primitives::chip_clickable("打开目录", false, false)
                    .id("model-open-dir")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.open_models_dir(cx);
                    })),
            );
            // 数据备份 / 恢复：与「模型与组件」同卡，因为两者都是「我机器上的本地
            // 状态」。备份走 `utils::backup`（连 WAL 一起打包），恢复带二次确认且
            // 底层拒绝覆盖现有文件——见 `actions.rs::backup_data` 的说明。
            row = row.child(
                primitives::chip_clickable("备份数据", false, false)
                    .id("data-backup-btn")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.backup_data(cx);
                    })),
            );
            row = row.child(
                primitives::chip_clickable("恢复备份", false, false)
                    .id("data-restore-btn")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.choose_backup_to_restore(cx);
                    })),
            );
            // 诊断报告：放在这一排的末尾，因为它回答的是同一类问题——「我机器上
            // 到底解析到了什么」。用户遇到「说缺 X 但我明明有 X」时第一件事就是导它。
            row = row.child(
                primitives::chip_clickable("导出诊断", false, false)
                    .id("data-diagnostics-btn")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.export_diagnostics(cx);
                    })),
            );
            // 检查更新：只在用户点击时发一次出站请求（不轮询——这个项目的卖点之一
            // 就是完全本地运行，而且 GitHub 未认证 API 有速率限制）。
            let checking = self.state.update_check_busy;
            row = row.child(
                primitives::chip_clickable(
                    if checking {
                        "检查中…"
                    } else {
                        "检查更新"
                    },
                    false,
                    false,
                )
                .id("update-check-btn")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.check_for_update(cx);
                })),
            );
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
        card = card.child(bulk_row);
        // 有新版本时给一个直达下载页的入口。只报告、不自动下载替换 exe——
        // 静默替换正在运行的可执行文件是能把安装搞死的操作。
        if let Some((true, text, url)) = self.state.update_check_result.clone() {
            let has_url = !url.trim().is_empty();
            card = card.child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap(px(Theme::SPACE_2))
                    .child(
                        div()
                            .text_size(px(Theme::TEXT_SMALL))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(Theme::accent_mint())
                            .child(text),
                    )
                    .children(has_url.then(|| {
                        primitives::chip_clickable("前往下载", false, false)
                            .id("update-open-page-btn")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.open_release_page(cx);
                            }))
                    })),
            );
        }
        card = card.child(list);
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
        // ElementId 只实现了 (&'static str, usize/u32/u64/EntityId) 等组合，
        // `(&str, &str)` 不在其中；用条目在 ITEMS 里的下标做唯一键（id 本就唯一，
        // 下标同样唯一且是 Copy 的 usize，可直接进 move 闭包）。
        let row_idx = ITEMS.iter().position(|i| i.id == item.id).unwrap_or(0);

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

        // 管理动作：定位（常驻）+ 删除（仅已就位且可安全删除时）。
        // 删除按钮**只对 `item_is_deletable` 为真的条目渲染**——该判定与
        // `delete_item_file` 共用同一条规则，不会出现「按钮点了却告诉你删不了」。
        // 压缩包类组件（llama.cpp / whisper-cli 这类「小 exe + 一堆 DLL」）不渲染
        // 删除按钮：它们解压到 `tools/` 共享目录，删单个文件只会留下坏掉的半成品。
        //
        // 为什么删除要二次确认：模型动辄几百 MB、重下要走网络，误触代价高，
        // 且它与视频库删除一样「一点即不可逆」，沿用同一个确认弹窗范式。
        let mut actions = div()
            .flex()
            .items_center()
            .flex_shrink_0()
            .gap(px(Theme::SPACE_1_5));
        actions = actions.child(
            primitives::mini_btn("定位", true)
                .id(("model-reveal", row_idx))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.reveal_model_file(row_id, cx);
                })),
        );
        if present && crate::utils::model_download::item_is_deletable(item.id) {
            let is_self_build = self.state.model_is_custom_build(item.id);
            let can_delete = !is_downloading && !is_self_build;
            let label = if is_self_build { "自编译" } else { "删除" };
            actions = actions.child(
                primitives::mini_btn(label, can_delete)
                    .id(("model-delete", row_idx))
                    .when(can_delete, |d| {
                        d.on_click(cx.listener(move |this, _, _, cx| {
                            let name = item.label.to_string();
                            this.confirm_dialog = Some(crate::ui::types::ConfirmDialogInfo {
                                title: format!("删除「{name}」？"),
                                message: format!(
                                    "将删除本地文件：{}。删除后可随时重新下载，已完成的转写工程不受影响。",
                                    crate::utils::AppConfig::resolve_path(item.dest).display()
                                ),
                                confirm_label: "删除".to_string(),
                                danger: true,
                                action: crate::ui::types::ConfirmAction::DeleteModelFile(
                                    item.id.to_string(),
                                ),
                            });
                            cx.notify();
                        }))
                    }),
            );
        }

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
            .child(
                div()
                    .flex()
                    .items_center()
                    .flex_shrink_0()
                    .gap(px(Theme::SPACE_2))
                    .child(status_el)
                    .child(actions),
            )
            .into_any_element()
    }
}
