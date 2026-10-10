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
use crate::utils::ITEMS;

// 体积格式化统一走 `utils::model_download::human_size`（单一实现处）：
use crate::utils::model_download::human_size;

/// 判断某个组件在用户当前的硬件配置和引擎模式下，是否属于「当前模式所需」的组件。
pub fn is_item_needed_by_current_config(item_id: &str, state: &crate::app::AppState) -> bool {
    let is_gpu = state.config.gpu.is_gpu_tier();
    let is_sv = state.whisper_model_tier == crate::app::WhisperModelTier::SenseVoice;

    match item_id {
        // 音视频抽音解码：任何模式都需要
        "ffmpeg" => true,

        // GPU 专用的 CUDA 推理程序：只有在 GPU 加速模式下才需要！纯 CPU 模式绝不需要
        "whisper-cublas" => is_gpu,

        // CPU 版本的 Whisper CLI：在纯 CPU 模式下需要
        "whisper-cli" => !is_gpu,

        // SenseVoice 引擎套件：选择 SenseVoice 极速模式时需要
        "sensevoice-model" | "sensevoice-tokens" | "sensevoice-vad" => is_sv,

        // Whisper 模型档位与 Silero VAD：选择 Whisper 全能模式时需要
        "whisper-small" | "whisper-base" | "whisper-turbo-q5" | "whisper-turbo-q8"
        | "silero-vad" => {
            if is_sv {
                false
            } else {
                if item_id == "silero-vad" || item_id == "whisper-small" {
                    true
                } else if let Some(current_id) = state.whisper_model_tier.download_item_id() {
                    item_id == current_id
                } else {
                    false
                }
            }
        }

        // 标点模型：开启标点恢复时需要
        "punc-model" => state.enable_polish,

        // 本地大模型：只有在本地离线 Qwen 翻译或 Qwen 深度润色模式下需要
        "qwen-llm" | "llama-cpp" => {
            state.config.translate.mode == "offline_qwen"
                || (state.enable_polish && state.polish_mode == crate::app::PolishMode::QwenDeep)
        }

        _ => true,
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
        let show_all = self.model_manager_show_all;
        let is_gpu = self.state.config.gpu.is_gpu_tier();

        // 进度比例：服务端没给 Content-Length 时退化为「不确定」态（用 0 表示）
        let ratio = if total > 0 {
            (done as f32 / total as f32).clamp(0.0, 1.0)
        } else {
            0.0
        };

        // ── 顶部：基于当前配置的精准缺失判定（纯 CPU 模式绝不把 GPU CUDA 组件算缺失） ──
        let active_missing_items: Vec<&crate::utils::DownloadItem> = ITEMS
            .iter()
            .filter(|i| !self.state.model_is_present(i.id))
            .filter(|i| is_item_needed_by_current_config(i.id, &self.state))
            .collect();
        let missing = active_missing_items.len();
        let missing_required = active_missing_items.iter().filter(|i| i.required).count();

        let missing_badge = if missing == 0 {
            primitives::badge_accent(if is_gpu {
                "当前模式组件已齐备 (GPU加速)"
            } else {
                "当前模式组件已齐备 (纯CPU模式)"
            })
        } else if missing == 1 {
            let item = active_missing_items[0];
            let tag = if item.required {
                "缺必需组件"
            } else {
                "缺可选组件"
            };
            primitives::badge_danger(format!("{tag}: {}", item.label))
        } else if missing <= 3 {
            let names = active_missing_items
                .iter()
                .map(|i| i.label)
                .collect::<Vec<_>>()
                .join("、");
            primitives::badge_danger(format!("缺 {missing} 项: {names}"))
        } else if missing_required > 0 {
            primitives::badge_danger(format!("缺 {missing} 项 (含 {missing_required} 个必需)"))
        } else {
            primitives::badge_danger(format!("缺 {missing} 个可选"))
        };

        // 视图切换胶囊：用啥显示啥 vs 全部组件库
        let view_toggle = primitives::segmented_cluster()
            .child(
                primitives::segmented("当前配置所需", !show_all, false)
                    .id("model-view-needed")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.model_manager_show_all = false;
                        cx.notify();
                    })),
            )
            .child(
                primitives::segmented("全部组件库", show_all, false)
                    .id("model-view-all")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.model_manager_show_all = true;
                        cx.notify();
                    })),
            );

        let header = div()
            .w_full()
            .flex()
            .items_center()
            .justify_between()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(Theme::SPACE_2))
                    .child(primitives::section_title("模型与组件"))
                    .child(missing_badge),
            )
            .child(view_toggle);

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

        // ── 分类别、分等级展示 (必需组件最前置) ──
        struct ModelCategory {
            title: &'static str,
            is_required: bool,
            item_ids: &'static [&'static str],
        }

        let categories = [
            ModelCategory {
                title: "核心必需组件",
                is_required: true,
                item_ids: &["ffmpeg", "whisper-cli", "whisper-small"],
            },
            ModelCategory {
                title: "GPU 硬件加速推理程序 (NVIDIA / Vulkan)",
                is_required: false,
                item_ids: &["whisper-cublas"],
            },
            ModelCategory {
                title: "Whisper 识别模型 (阶梯分级)",
                is_required: false,
                item_ids: &["whisper-base", "whisper-turbo-q5", "whisper-turbo-q8"],
            },
            ModelCategory {
                title: "端到端语音与辅助检测",
                is_required: false,
                item_ids: &[
                    "sensevoice-model",
                    "sensevoice-tokens",
                    "sensevoice-vad",
                    "silero-vad",
                    "punc-model",
                ],
            },
            ModelCategory {
                title: "本地大语言模型与推理程序",
                is_required: false,
                item_ids: &["qwen-llm", "llama-cpp"],
            },
        ];

        let mut list = div().w_full().flex().flex_col().gap(px(Theme::SPACE_3));
        for cat in categories {
            let cat_items: Vec<_> = cat
                .item_ids
                .iter()
                .filter_map(|&id| ITEMS.iter().find(|i| i.id == id))
                .filter(|i| {
                    if !show_all {
                        // 「当前配置所需」模式：用啥显示啥，CPU 模式下绝不展示未开启的 GPU 组件
                        is_item_needed_by_current_config(i.id, &self.state)
                    } else {
                        // 「全部组件库」模式：展示全部项
                        !compact || !self.state.model_is_present(i.id)
                    }
                })
                .collect();

            if cat_items.is_empty() {
                continue;
            }

            // 当前分类下真正缺失的条目（当前不需要的不计入缺失警示）
            let cat_missing = cat_items
                .iter()
                .filter(|i| {
                    !self.state.model_is_present(i.id)
                        && is_item_needed_by_current_config(i.id, &self.state)
                })
                .count();

            let mut col = div().w_full().flex().flex_col().gap(px(Theme::SPACE_1_5));
            let mut title_row = div().flex().items_center().gap_2().child(
                div()
                    .text_size(px(Theme::TEXT_BODY))
                    .font_weight(FontWeight::BOLD)
                    .text_color(if cat_missing > 0 {
                        Theme::accent_red()
                    } else if cat.is_required {
                        Theme::accent_mint()
                    } else {
                        Theme::text_secondary()
                    })
                    .child(cat.title),
            );
            if cat.is_required {
                title_row = title_row.child(primitives::badge_accent("必需"));
            }
            if cat_missing > 0 {
                title_row =
                    title_row.child(primitives::badge_danger(format!("缺 {cat_missing} 项")));
            }
            col = col.child(title_row);
            for item in cat_items {
                col = col.child(self.render_model_row(item, is_downloading, cx));
            }
            list = list.child(col);
        }

        // ── 组装 ──
        let mut card = primitives::card_sm().gap(px(Theme::SPACE_2)).child(header);
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

    /// 单个条目行：名称 + 体积 + 明确状态徽标（已就位/未下载） + 操作区。
    fn render_model_row(
        &mut self,
        item: &'static crate::utils::DownloadItem,
        is_downloading: bool,
        cx: &mut gpui::Context<Self>,
    ) -> AnyElement {
        let present = self.state.model_is_present(item.id);
        let custom = present && self.state.model_is_custom_build(item.id);
        let is_current = self
            .state
            .download_current
            .as_deref()
            .map(|c| c == item.id)
            .unwrap_or(false);
        let row_id = item.id;
        let row_idx = ITEMS.iter().position(|i| i.id == item.id).unwrap_or(0);

        let is_needed = is_item_needed_by_current_config(item.id, &self.state);

        // 状态徽标：直接紧跟在名称和体积右侧，任何分辨率下都一眼可见
        let status_badge = if is_current && is_downloading {
            primitives::badge_accent("下载中")
        } else if custom {
            primitives::badge_accent("已就位 · 自编译")
        } else if present {
            primitives::badge_accent("已就位")
        } else if !is_needed {
            primitives::badge("未下载 · 当前模式无需此项")
        } else if item.required {
            primitives::badge_danger("未下载 · 必需")
        } else {
            primitives::badge_danger("未下载")
        };

        // 操作区：未就位显示下载按钮；已就位显示「定位」与「删除」
        let right_actions = if !present {
            let downloading_this = is_current && is_downloading;
            div()
                .flex()
                .items_center()
                .flex_shrink_0()
                .gap(px(Theme::SPACE_1_5))
                .child(
                    primitives::btn_clickable(
                        if downloading_this {
                            "正在下载…"
                        } else if is_needed {
                            "立即下载"
                        } else {
                            "下载 (备用)"
                        },
                        primitives::BtnSize::Sm,
                        if is_needed {
                            primitives::BtnVariant::Primary
                        } else {
                            primitives::BtnVariant::Secondary
                        },
                    )
                    .id(row_id)
                    .when(!is_downloading, |s| {
                        s.on_click(cx.listener(move |this, _, _, cx| {
                            this.start_model_download(row_id, cx);
                        }))
                    }),
                )
        } else {
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
            if crate::utils::model_download::item_is_deletable(item.id) {
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
            actions
        };

        let mut row_container = div()
            .w_full()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(Theme::SPACE_2))
            .px(px(Theme::SPACE_2))
            .py(px(Theme::SPACE_1_5))
            .rounded(px(Theme::RADIUS_SM));

        if !present && !is_downloading && is_needed {
            row_container = row_container
                .bg(Theme::tint_red_soft())
                .border_1()
                .border_color(Theme::tint_red_border());
        } else {
            row_container = row_container.hover(|s| s.bg(Theme::bg_hover()));
        }

        row_container
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(Theme::SPACE_0_5))
                    .min_w(px(0.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(Theme::SPACE_1_5))
                            .child(
                                div()
                                    .text_size(px(Theme::TEXT_SMALL))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(if !present {
                                        Theme::accent_red()
                                    } else {
                                        Theme::text_primary()
                                    })
                                    .child(item.label),
                            )
                            .child(
                                div()
                                    .text_size(px(Theme::TEXT_CAPTION))
                                    .text_color(Theme::text_muted())
                                    .child(human_size(item.size)),
                            )
                            .child(status_badge),
                    ),
            )
            .child(right_actions)
            .into_any_element()
    }
}
