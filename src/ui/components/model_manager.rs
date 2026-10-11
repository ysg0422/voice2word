//! 模型与外部组件管理面板（首次使用引导与模型库）。
//!
//! # 架构设计
//!
//! 1. **零冗余依赖**：SenseVoice 默认模式为纯自包含 ONNX 架构，无需任何 whisper.cpp / llama.cpp；
//!    在线 API 翻译模式无需本地 Qwen / llama.cpp。
//! 2. **镜像加速**：优先直连阿里云魔搭社区（ModelScope CDN，实测 10MB/s+），自动回落 HuggingFace 镜像。
//! 3. **视觉层次优化**：核心模型与底层执行程序分流展示，系统维护工具独立沉底。

use gpui::prelude::*;
use gpui::{div, px, AnyElement, FontWeight, IntoElement, ParentElement, Styled};

use crate::ui::primitives;
use crate::ui::theme::Theme;
use crate::ui::{MainWindow, ModelManagerTab};
use crate::utils::model_download::human_size;
use crate::utils::ITEMS;

/// 判断某个组件在用户选择的分类专区下是否需要展示或下载。
pub(crate) fn is_item_needed_for_tab(
    item_id: &str,
    tab: ModelManagerTab,
    state: &crate::app::AppState,
) -> bool {
    let is_gpu = state.config.gpu.is_gpu_tier();
    match tab {
        ModelManagerTab::SenseVoice => {
            // SenseVoice 专区：核心识别模型、分词表、VAD 专属项
            matches!(
                item_id,
                "sensevoice-model" | "sensevoice-tokens" | "sensevoice-vad"
            )
        }
        ModelManagerTab::Whisper => {
            // Whisper 专区：Whisper 模型档位、VAD、标点、推理程序
            if item_id == "whisper-cublas" {
                is_gpu
            } else if item_id == "whisper-cli" {
                !is_gpu
            } else if item_id == "silero-vad" || item_id == "punc-model" {
                true
            } else if let Some(current_id) = state.whisper_model_tier.download_item_id() {
                item_id == current_id
            } else {
                item_id == "whisper-turbo-q5" || item_id == "whisper-small"
            }
        }
        ModelManagerTab::Common => {
            // 公用组件：FFmpeg 媒体流提取、yt-dlp、本地翻译大模型及 llama.cpp
            matches!(
                item_id,
                "ffmpeg" | "yt-dlp" | "llama-cpp" | "qwen-llm"
            )
        }
    }
}

/// 判断某个组件在用户当前的硬件配置和引擎模式下，是否属于「当前模式所需」的组件。
pub fn is_item_needed_by_current_config(item_id: &str, state: &crate::app::AppState) -> bool {
    let is_gpu = state.config.gpu.is_gpu_tier();
    let is_sv = state.whisper_model_tier == crate::app::WhisperModelTier::SenseVoice;

    match item_id {
        // 音视频抽音解码：任何模式都需要
        "ffmpeg" => true,

        // GPU 专用的 CUDA 推理程序：只有在选择 Whisper 且开启 GPU 加速模式下才需要！
        "whisper-cublas" => is_gpu && !is_sv,

        // CPU 版本的 Whisper CLI：只有在选择 Whisper 且纯 CPU 模式下才需要！
        "whisper-cli" => !is_gpu && !is_sv,

        // SenseVoice 引擎套件：选择 SenseVoice 极速模式时需要（自包含 ONNX 架构，无需任何 cpp）
        "sensevoice-model" | "sensevoice-tokens" | "sensevoice-vad" => is_sv,

        // Whisper 模型档位与 Silero VAD：选择 Whisper 模式时需要
        "whisper-small" | "whisper-base" | "whisper-turbo-q5" | "whisper-turbo-q8"
        | "silero-vad" => {
            if is_sv {
                false
            } else if item_id == "silero-vad" {
                true
            } else if let Some(current_id) = state.whisper_model_tier.download_item_id() {
                item_id == current_id
            } else {
                item_id == "whisper-small"
            }
        }

        // 标点模型：开启标点恢复时需要
        "punc-model" => state.enable_polish,

        // 本地离线大模型：只有在本地离线 Qwen 翻译或 Qwen 深度润色模式下需要
        "qwen-llm" | "llama-cpp" => {
            state.config.translate.mode == "offline_qwen"
                || (state.enable_polish && state.polish_mode == crate::app::PolishMode::QwenDeep)
        }

        _ => false,
    }
}

impl MainWindow {
    /// 模型管理卡片。`compact` 为真时只显示缺失项与一个「一键补齐」按钮
    /// （用于侧栏），否则显示完整清单（用于独立页面）。
    pub(crate) fn render_model_manager(
        &mut self,
        _compact: bool,
        cx: &mut gpui::Context<Self>,
    ) -> AnyElement {
        let is_downloading = self.state.is_downloading;
        let done = self.state.download_done_bytes;
        let total = self.state.download_total_bytes;
        let status = self.state.download_status_msg.clone();
        let current = self.state.download_current.clone();
        let current_tab = self.model_manager_tab;
        let is_gpu = self.state.config.gpu.is_gpu_tier();

        // 进度比例：服务端没给 Content-Length 时退化为「不确定」态（用 0 表示）
        let ratio = if total > 0 {
            (done as f32 / total as f32).clamp(0.0, 1.0)
        } else {
            0.0
        };

        // ── 顶部：基于当前选定专区计算缺失项 ──
        let active_missing_items: Vec<&crate::utils::DownloadItem> = ITEMS
            .iter()
            .filter(|i| !self.state.model_is_present(i.id))
            .filter(|i| is_item_needed_for_tab(i.id, current_tab, &self.state))
            .collect();
        let missing = active_missing_items.len();

        let missing_badge = if missing == 0 {
            primitives::badge_accent(match current_tab {
                ModelManagerTab::SenseVoice => "SenseVoice 组件已就绪 (推荐引擎)",
                ModelManagerTab::Whisper => "Whisper 所需组件已就绪",
                ModelManagerTab::Common => "公用支撑组件已就绪",
            })
        } else if missing == 1 {
            let item = active_missing_items[0];
            primitives::badge_danger(format!("缺组件: {}", item.label))
        } else if missing <= 3 {
            let names = active_missing_items
                .iter()
                .map(|i| i.label)
                .collect::<Vec<_>>()
                .join("、");
            primitives::badge_danger(format!("缺 {missing} 项: {names}"))
        } else {
            primitives::badge_danger(format!("缺 {missing} 项"))
        };

        // 视图切换胶囊：SenseVoice (推荐) vs Whisper 模型库 vs 公用组件
        let view_toggle = primitives::segmented_cluster()
            .child(
                primitives::segmented(
                    "SenseVoice (推荐)",
                    current_tab == ModelManagerTab::SenseVoice,
                    false,
                )
                .id("model-tab-sv")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.model_manager_tab = ModelManagerTab::SenseVoice;
                    cx.notify();
                })),
            )
            .child(
                primitives::segmented(
                    "Whisper 模型库",
                    current_tab == ModelManagerTab::Whisper,
                    false,
                )
                .id("model-tab-whisper")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.model_manager_tab = ModelManagerTab::Whisper;
                    cx.notify();
                })),
            )
            .child(
                primitives::segmented("公用组件", current_tab == ModelManagerTab::Common, false)
                    .id("model-tab-common")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.model_manager_tab = ModelManagerTab::Common;
                        cx.notify();
                    })),
            );

        // 占用统计
        let presence = crate::utils::model_download::PresenceContextRef::new(&self.state.config);
        let (used_bytes, used_count) = ITEMS
            .iter()
            .filter(|i| presence.is_present(i))
            .filter_map(|i| {
                let p = crate::utils::model_download::item_effective_path(i, &self.state.config);
                crate::utils::model_download::disk_size(&p)
            })
            .fold((0u64, 0usize), |(bytes, n), sz| (bytes + sz, n + 1));

        let header = div()
            .w_full()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(Theme::SPACE_2))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(Theme::SPACE_2_5))
                    .child(primitives::section_title("模型与执行组件"))
                    .child(missing_badge)
                    .when(used_count > 0, |d| {
                        d.child(
                            div()
                                .text_size(px(Theme::TEXT_CAPTION))
                                .text_color(Theme::text_muted())
                                .child(format!(
                                    "已就位 {used_count} 项 · 占 {}",
                                    human_size(used_bytes)
                                )),
                        )
                    }),
            )
            .child(view_toggle);

        // ── 主操作动作条（补齐所需 / 取消下载） ──
        let action_bar = div()
            .w_full()
            .flex()
            .items_center()
            .justify_end()
            .gap(px(Theme::SPACE_2))
            .children(if is_downloading {
                Some(
                    primitives::btn_clickable(
                        "取消下载",
                        primitives::BtnSize::Sm,
                        primitives::BtnVariant::Secondary,
                    )
                    .id("model-dl-cancel")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.model_download_cancel
                            .store(true, std::sync::atomic::Ordering::SeqCst);
                        this.state.download_status_msg = "正在取消下载…".to_string();
                        cx.notify();
                    })),
                )
            } else if missing > 0 {
                Some(
                    primitives::btn_clickable(
                        match current_tab {
                            ModelManagerTab::SenseVoice => {
                                format!("补齐 SenseVoice 所需 ({missing} 项)")
                            }
                            ModelManagerTab::Whisper => {
                                format!("补齐 Whisper 所需 ({missing} 项)")
                            }
                            ModelManagerTab::Common => {
                                format!("补齐公用组件 ({missing} 项)")
                            }
                        },
                        primitives::BtnSize::Sm,
                        primitives::BtnVariant::Primary,
                    )
                    .id("model-dl-all")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.download_all_missing(cx);
                    })),
                )
            } else {
                None
            });

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
                    .gap(px(Theme::SPACE_1_5))
                    .py(px(Theme::SPACE_1))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_size(px(Theme::TEXT_SMALL))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(Theme::accent_mint())
                                    .child(label),
                            )
                            .when(total > 0, |d| {
                                d.child(
                                    div()
                                        .text_size(px(Theme::TEXT_CAPTION))
                                        .text_color(Theme::text_muted())
                                        .child(format!("{:.1}%", ratio * 100.0)),
                                )
                            }),
                    )
                    .child(if total > 0 {
                        primitives::progress_bar(ratio)
                    } else {
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

        // ── 分类结构 ──
        struct ModelCategory {
            title: &'static str,
            item_ids: &'static [&'static str],
        }

        let categories = match current_tab {
            ModelManagerTab::SenseVoice => vec![ModelCategory {
                title: "SenseVoice 极速语音识别 (推荐)",
                item_ids: &[
                    "sensevoice-model",
                    "sensevoice-tokens",
                    "sensevoice-vad",
                ],
            }],
            ModelManagerTab::Whisper => vec![
                ModelCategory {
                    title: "Whisper 识别模型库",
                    item_ids: &[
                        "whisper-turbo-q5",
                        "whisper-turbo-q8",
                        "whisper-small",
                        "whisper-base",
                        "silero-vad",
                        "punc-model",
                    ],
                },
                ModelCategory {
                    title: "Whisper 推理程序",
                    item_ids: if is_gpu {
                        &["whisper-cublas"]
                    } else {
                        &["whisper-cli"]
                    },
                },
            ],
            ModelManagerTab::Common => vec![
                ModelCategory {
                    title: "音视频解码与流媒体解析",
                    item_ids: &["ffmpeg", "yt-dlp"],
                },
                ModelCategory {
                    title: "本地大模型翻译与润色引擎",
                    item_ids: &["llama-cpp", "qwen-llm"],
                },
            ],
        };

        let mut list = div().w_full().flex().flex_col().gap(px(Theme::SPACE_3));
        for cat in categories {
            let cat_items: Vec<_> = cat
                .item_ids
                .iter()
                .filter_map(|&id| ITEMS.iter().find(|i| i.id == id))
                .filter(|i| {
                    is_item_needed_for_tab(i.id, current_tab, &self.state)
                })
                .collect();

            if cat_items.is_empty() {
                continue;
            }

            let cat_missing = cat_items
                .iter()
                .filter(|i| {
                    !self.state.model_is_present(i.id)
                        && is_item_needed_for_tab(i.id, current_tab, &self.state)
                })
                .count();

            let mut col = div().w_full().flex().flex_col().gap(px(Theme::SPACE_1_5));
            let title_row = div()
                .flex()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .text_size(px(Theme::TEXT_BODY))
                        .font_weight(FontWeight::BOLD)
                        .text_color(if cat_missing > 0 {
                            Theme::accent_red()
                        } else {
                            Theme::text_primary()
                        })
                        .child(cat.title),
                )
                .when(cat_missing > 0, |d| {
                    d.child(primitives::badge_danger(format!("缺 {cat_missing} 项")))
                });

            col = col.child(title_row);
            for item in cat_items {
                col = col.child(self.render_model_row(item, is_downloading, cx));
            }
            list = list.child(col);
        }

        // ── 底部维护工具栏 ──
        let checking = self.state.update_check_busy;
        let footer_tools = div()
            .w_full()
            .pt(px(Theme::SPACE_2))
            .border_t_1()
            .border_color(Theme::border())
            .flex()
            .items_center()
            .justify_end()
            .gap(px(Theme::SPACE_1_5))
            .child(
                primitives::chip_clickable("打开目录", false, false)
                    .id("model-open-dir")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.open_models_dir(cx);
                    })),
            )
            .child(
                primitives::chip_clickable("备份数据", false, false)
                    .id("data-backup-btn")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.backup_data(cx);
                    })),
            )
            .child(
                primitives::chip_clickable("恢复备份", false, false)
                    .id("data-restore-btn")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.choose_backup_to_restore(cx);
                    })),
            )
            .child(
                primitives::chip_clickable("导出诊断", false, false)
                    .id("data-diagnostics-btn")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.export_diagnostics(cx);
                    })),
            )
            .child(
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

        // ── 组装整张卡片 ──
        let mut card = primitives::card_sm()
            .gap(px(Theme::SPACE_2_5))
            .child(header)
            .child(action_bar);

        if let Some(p) = progress {
            card = card.child(p);
        }

        // 版本更新横幅（如果有）
        if let Some((true, text, _url)) = self.state.update_check_result.clone() {
            let is_downloading = self.state.update_download_busy;
            let downloaded = self.state.update_downloaded_path.is_some();
            let has_target = self.state.update_download_target.is_some();
            card = card.child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(Theme::SPACE_2))
                    .child(
                        div()
                            .text_size(px(Theme::TEXT_SMALL))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(Theme::accent_mint())
                            .child(if let Some(status) = &self.state.update_download_status {
                                status.clone()
                            } else {
                                text
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .children(if downloaded {
                                Some(
                                    primitives::chip_clickable("打开新版本", true, false)
                                        .id("update-open-file-btn")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.open_downloaded_update(cx);
                                        })),
                                )
                            } else if is_downloading {
                                Some(
                                    primitives::chip_clickable("正在下载…", false, false)
                                        .id("update-downloading-btn"),
                                )
                            } else if has_target {
                                Some(
                                    primitives::chip_clickable("直接下载更新", true, false)
                                        .id("update-direct-download-btn")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.download_app_update(cx);
                                        })),
                                )
                            } else {
                                Some(
                                    primitives::chip_clickable("前往下载", false, false)
                                        .id("update-open-page-btn")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.open_release_page(cx);
                                        })),
                                )
                            }),
                    ),
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

        card = card.child(footer_tools);
        card.into_any_element()
    }

    /// 单个条目行：名称 + 用途说明 + 体积 + 状态徽标 + 操作区。
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

        // 状态徽标：已就位 / 必需 / 可选
        let status_badge = if is_current && is_downloading {
            primitives::badge_accent("下载中")
        } else if custom {
            primitives::badge_accent("已就位 (自编译)")
        } else if present {
            primitives::badge_accent("已就位")
        } else if !is_needed {
            primitives::badge("未下载 (当前模式无需)")
        } else if item.required {
            primitives::badge_danger("核心依赖 · 待补齐")
        } else {
            primitives::badge_danger("待下载")
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
                            "下载备用"
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
            .gap(px(Theme::SPACE_3))
            .px(px(Theme::SPACE_3))
            .py(px(Theme::SPACE_2))
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
                    .flex_1()
                    .flex()
                    .flex_col()
                    .gap(px(Theme::SPACE_0_5))
                    .min_w(px(0.0))
                    // 上行：标题 + 体积 + 徽章
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(Theme::SPACE_2))
                            .child(
                                div()
                                    .text_size(px(Theme::TEXT_BODY))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(if !present && is_needed {
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
