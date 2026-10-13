//! 右侧转写参数配置抽屉及硬件监控部件
//!
//! 结构：标题 → 文件卡片 → 识别引擎档位栅格 → 转写参数（语言/格式/润色/说话人/线程/热词）→ 硬件监控 → 主操作 CTA

use gpui::prelude::*;
use gpui::*;

use super::super::primitives;
use super::super::theme::Theme;
use super::super::MainWindow;
use crate::app::state::{ProcessStatus, WorkspaceTab};
use crate::app::{PolishMode, WhisperModelTier};
use crate::engines::MAX_SPEAKERS;
use crate::utils::time::format_duration_short;

/// 说话人数量预设表：`(选项值, 显示名)`，从 2 人起（引擎收敛区间的下限也是 2）。
/// 渲染时只取前 `MAX_SPEAKERS - 1` 项，所以这张表只需要「够长」，多余的项不会出现在界面上。
const SPEAKER_PRESETS: [(&str, &str); 6] = [
    ("2", "2 人"),
    ("3", "3 人"),
    ("4", "4 人"),
    ("5", "5 人"),
    ("6", "6 人"),
    ("7", "7 人"),
];

/// 编译期校验：预设表必须覆盖 `2..=MAX_SPEAKERS`。
///
/// 这条约束以前写成运行时的 `debug_assert_eq!`，而且判定条件本身写错了
/// （拿「选项数 - 1」去比 `MAX_SPEAKERS`，等于要求界面提供 2..=5 人）。
/// 结果每次点开「视频转写」渲染右侧配置面板都会 panic，程序直接退出。
/// 改成 const 断言后，条件不对是编译不过，而不是用户点一下界面就崩。
const _: () = assert!(
    MAX_SPEAKERS >= 2 && (MAX_SPEAKERS as usize) - 1 <= SPEAKER_PRESETS.len(),
    "SPEAKER_PRESETS 条目数不足以覆盖 2..=MAX_SPEAKERS，请补齐后重试"
);

/// 说话人分离的选项列表：`关闭` + `2..=MAX_SPEAKERS` 人。
fn speaker_options() -> Vec<(&'static str, &'static str)> {
    let mut options = Vec::with_capacity(MAX_SPEAKERS as usize);
    options.push(("off", "关闭"));
    options.extend(
        SPEAKER_PRESETS[..(MAX_SPEAKERS as usize) - 1]
            .iter()
            .copied(),
    );
    options
}

/// 「转写线程」候选项：与性能页滑条同口径（`2..=本机逻辑核心数`），
/// 取 2 的幂次常用档位，并显式并入滑条上限（核数）与配置里的当前值。
///
/// 为什么必须并入当前值：抽屉此前把选项写死成 4/8/16，而性能页滑条是按核数
/// 推导的（`views/performance.rs`），配置里可能是 12 这类不在旧表里的值；那时
/// 抽屉里三个胶囊全部处于未选中态，看起来像「什么都没选」，两处口径不一致。
/// 候选项与性能页共用同一个上限来源（[`crate::app::AppState::logical_cores`]）。
fn thread_presets(current: u32, cores: u32) -> Vec<u32> {
    let max = cores.max(2);
    let mut values: Vec<u32> = [2u32, 4, 8, 16, 32]
        .into_iter()
        .filter(|v| *v <= max)
        .collect();
    // 滑条能达到的上限档，保证抽屉里也能直接选到它
    values.push(max);
    // 配置里的当前值必须始终在列，否则胶囊会全部落空
    values.push(current.clamp(2, max));
    values.sort_unstable();
    values.dedup();
    values
}

/// 「识别引擎」档位的静态文案：`(档位, 一句话定位)`。
///
/// 名字与体积不写死在表里：名字取 `WhisperModelTier::label()`、体积取下载清单
/// （`utils::model_download`），两处都是唯一实现——同一档位在抽屉和性能页显示成
/// 不同的名字或数字是很容易出现的分叉，这里从源头避免。
const TIER_TAGLINES: [(WhisperModelTier, &str); 5] = [
    (
        WhisperModelTier::SenseVoice,
        "中文首选 · 不用显卡也能飞快出字",
    ),
    (WhisperModelTier::Fast, "最快 · 低配机器也跑得动"),
    (WhisperModelTier::Balanced, "均衡 · 出厂默认，多数视频够用"),
    (
        WhisperModelTier::TurboSpeed,
        "较准 · 比默认更快更准，综合推荐",
    ),
    (WhisperModelTier::Precise, "最准 · 抗口音吞音，也最慢"),
];

/// SenseVoice 档对应的一组下载条目：模型 + 词表 + VAD，三个文件缺一不可。
const SENSEVOICE_ITEM_IDS: [&str; 3] = ["sensevoice-model", "sensevoice-tokens", "sensevoice-vad"];

/// 「怎么选？」提示条的展开态。
///
/// 用静态变量而不是给 `MainWindow` 加字段：那个结构定义在 `ui/mod.rs`，是并行改动
/// 最集中的文件；而这里是纯界面开合态，程序重启后默认收起完全可以接受。
static HOW_TO_CHOOSE_OPEN: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// 「怎么选？」的三行结论。抽屉只有 350px 宽，只讲「选哪个」，不讲原理。
const HOW_TO_CHOOSE_LINES: [&str; 3] = [
    "中文播客、短视频：SenseVoice 极速，快且够用",
    "要双语字幕或更准的断句：Whisper Turbo Q5",
    "机器一般：先下 Whisper Base / Small 跑通再升级",
];

/// 读取「怎么选？」的展开态。
///
/// 读写都收在这两个小函数里，是为了不用在文件顶部 glob 引入 `Ordering`——
/// 本文件有 `use gpui::*`，裸名 `Ordering` 是否与 gpui 的重名要靠编译才知道。
fn how_to_choose_open() -> bool {
    HOW_TO_CHOOSE_OPEN.load(std::sync::atomic::Ordering::Relaxed)
}

/// 切换「怎么选？」的展开态。
fn toggle_how_to_choose() {
    HOW_TO_CHOOSE_OPEN.fetch_xor(true, std::sync::atomic::Ordering::Relaxed);
}

/// 一组下载条目的合计体积文案：`约 229 MB`。
///
/// 一个条目都查不到时返回「体积未知」，而不是隐藏整块或 panic：清单里改了 id、
/// 删了条目都不该让卡片消失——用户至少要能看到这一档，只是体积未知。
fn bundle_size_text(ids: &[&str]) -> String {
    let bytes: u64 = ids
        .iter()
        .filter_map(|id| crate::utils::model_download::item_by_id(id))
        .map(|item| item.size)
        .sum();
    if bytes == 0 {
        "体积未知".to_string()
    } else {
        format!("约 {}", crate::utils::model_download::human_size(bytes))
    }
}

/// 档位卡片上显示的体积。
///
/// SenseVoice 不是单个文件（模型 / 词表 / VAD 三条目），只报模型那一条会低估用户
/// 实际要下载的量，所以按整组求和。
fn tier_size_text(tier: WhisperModelTier) -> String {
    match tier.download_item_id() {
        Some(id) => bundle_size_text(&[id]),
        None => bundle_size_text(&SENSEVOICE_ITEM_IDS),
    }
}

/// 档位状态徽标文案：下载中 > 已下载 > 需下载。
fn tier_status_text(present: bool, downloading: bool) -> &'static str {
    if downloading {
        "下载中…"
    } else if present {
        "已下载"
    } else {
        "需下载"
    }
}

/// 这一档名下是否有条目正在下载。
///
/// SenseVoice 没有单一条目 id（三个文件组成），因此按前缀判定：其中任何一个在下载
/// 都算这一档在下载，否则会出现「明明在下 SenseVoice 的词表，卡片却说需下载」。
fn tier_downloading(tier: WhisperModelTier, current: Option<&str>) -> bool {
    let Some(current) = current else {
        return false;
    };
    match tier.download_item_id() {
        Some(id) => current == id,
        None => current.starts_with("sensevoice"),
    }
}

/// 热词输入框的编辑态。
///
/// # 为什么放在线程局部而不是 `MainWindow` 字段
/// 本窗的界面状态惯例是挂在 `MainWindow` 上（见 `src/ui/mod.rs` 的 `api_base_input`、
/// `focused_api_field`），但本轮只允许改 `whisper.rs` / `config.rs` / `config_drawer.rs`，
/// 而 `MainWindow` 的定义在 `src/ui/mod.rs`（正被并发修改）。GPUI 在本仓是单线程的，
/// 线程局部的 `RefCell` 与「挂在 `MainWindow` 上」语义等价，只是搬了个家。
struct HotwordsInput {
    /// 输入框的焦点句柄。必须**跨帧复用同一个**：每帧 `cx.focus_handle()` 新建一个，
    /// `window.focus()` 就会指到一个已经不在渲染树里的焦点上，键盘输入随即丢失。
    focus: Option<FocusHandle>,
    /// 光标位置（字符下标，不是字节：自绘输入框一律按 `char` 计数）。
    cursor: usize,
    /// 输入框是否持有焦点。**只驱动外观**（描边高亮 + 光标块）——判断「这次按键该不该
    /// 被接管」用的是 `FocusHandle::is_focused(window)`，拦截器里拿得到 `Window`，更权威。
    focused: bool,
    /// 键盘拦截器句柄（一次性挂上，见 `MainWindow::ensure_hotwords_hooks`）。
    /// `Subscription` 一 drop 就等于注销，所以「只挂一次」的守卫得跟它一起常驻。
    key_hook: Option<Subscription>,
    /// 失焦订阅句柄（同上）。
    blur_hook: Option<Subscription>,
}

thread_local! {
    static HOTWORDS_INPUT: std::cell::RefCell<HotwordsInput> = const {
        std::cell::RefCell::new(HotwordsInput {
            focus: None,
            cursor: 0,
            focused: false,
            key_hook: None,
            blur_hook: None,
        })
    };
}

/// 焦点在框里时，光标左右各保留多少个可见字符。
///
/// 抽屉只有 350px 宽，而热词上限是 200 字符（`whisper.rs` 的 `MAX_ASR_HOTWORDS_CHARS`）：
/// 整串渲染会被 `.truncate()` 从尾部切掉，光标一旦跑到后面就再也看不见，用户就不知道自己
/// 在改哪一段。这里改成围绕光标的滑窗。
const HOTWORDS_VIEW_CHARS: usize = 19;

/// 围绕光标切出可见窗口：`(光标左侧, 光标右侧)`。
///
/// 两侧被切掉时补一个 `…`：没有这个记号，用户会以为「屏幕上的就是全部」，
/// 在长词表中改到中间一段时尤其容易看错。
fn hotwords_display_window(text: &str, cursor: usize) -> (String, String) {
    let chars: Vec<char> = text.chars().collect();
    let cursor = cursor.min(chars.len());
    let start = cursor.saturating_sub(HOTWORDS_VIEW_CHARS);
    let end = cursor.saturating_add(HOTWORDS_VIEW_CHARS).min(chars.len());
    let mut before = String::new();
    if start > 0 {
        before.push('…');
    }
    before.extend(&chars[start..cursor]);
    let mut after: String = chars[cursor..end].iter().collect();
    if end < chars.len() {
        after.push('…');
    }
    (before, after)
}

/// 裸键（空格 / 退格 / Delete）在热词输入框里的编辑核（纯函数，有单测）。
///
/// 为什么不直接复用 `crate::ui::apply_line_edit`：那条按 `keystroke.key` 判「可打印单字符」，
/// 而空格键的键名是 `"space"`（多字符），于是它插不进空格——热词里空格恰恰是常用分隔符，
/// 这一格只能由这里补上。返回 `true` 表示这个键由本函数处理掉了。
fn apply_hotwords_bare_key(buffer: &mut String, cursor: &mut usize, key: &str) -> bool {
    let mut chars: Vec<char> = buffer.chars().collect();
    let total = chars.len();
    *cursor = (*cursor).min(total);
    match key.to_ascii_lowercase().as_str() {
        "space" => {
            chars.insert(*cursor, ' ');
            *buffer = chars.into_iter().collect();
            *cursor += 1;
            true
        }
        "backspace" | "back" | "\u{8}" => {
            if *cursor > 0 {
                chars.remove(*cursor - 1);
                *cursor -= 1;
                *buffer = chars.into_iter().collect();
            }
            true
        }
        "delete" | "del" | "\u{7f}" => {
            if *cursor < total {
                chars.remove(*cursor);
                *buffer = chars.into_iter().collect();
            }
            true
        }
        _ => false,
    }
}

impl MainWindow {
    /// 渲染右侧配置面板 (转写配置与选项：左中右架构之「右」)
    pub(crate) fn render_sidebar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let is_sv = self.state.whisper_model_tier == WhisperModelTier::SenseVoice;
        let is_processing = matches!(self.state.status, ProcessStatus::Processing { .. });
        let has_file = self.state.transcribe_file.is_some();
        // 批量队列非空时，侧栏 CTA 的主语义从「转写这一个」变成「把这一批跑完」
        let has_batch = !self.state.batch_queue.is_empty();
        let batch_running = self.state.batch_running;
        let can_act = has_file || has_batch;
        let fname = self
            .state
            .transcribe_file
            .as_ref()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            .unwrap_or("未选择文件")
            .to_string();
        let dur_str = format_duration_short(self.state.transcribe_duration);

        div()
            .id("sidebar")
            .w(px(Theme::DRAWER_W))
            .flex_shrink_0()
            .h_full()
            .overflow_y_scroll()
            .bg(Theme::bg_sidebar())
            .border_l_1()
            .border_color(Theme::border())
            .p(px(Theme::PAGE_PAD))
            .flex()
            .flex_col()
            .gap(px(Theme::CARD_GAP))
            // ── 1. 顶栏标头与引擎指示 ──
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .pb_0p5()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(primitives::stat_dot(if is_processing {
                                Theme::accent_orange()
                            } else {
                                Theme::accent_mint()
                            }))
                            .child(primitives::panel_title("转写配置")),
                    )
                    .child(primitives::badge(if is_sv {
                        "SenseVoice"
                    } else {
                        "Whisper"
                    })),
            )
            // ── 2. 媒体文件选择与状态卡片 ──
            .child(
                primitives::card_sm()
                    .id("media-select-card")
                    // card_sm 默认纵向排布；这一张是「图标 + 文件名 + 按钮」的横排卡片
                    .flex_row()
                    .bg(Theme::bg_inset())
                    .border_1()
                    .border_color(if has_file {
                        Theme::tint_mint_border()
                    } else {
                        Theme::border()
                    })
                    .cursor_pointer()
                    .hover(|s| {
                        s.bg(Theme::bg_card_hover())
                            .border_color(Theme::border_light())
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.choose_file(cx);
                    }))
                    .flex()
                    .items_center()
                    .gap(px(Theme::SPACE_3))
                    .child(
                        div()
                            .w(px(Theme::CTRL_H_LG))
                            .h(px(Theme::CTRL_H_LG))
                            .rounded(px(Theme::RADIUS_LG))
                            .bg(if has_file {
                                Theme::tint_blue_soft()
                            } else {
                                Theme::tint_mint_soft()
                            })
                            .border_1()
                            .border_color(if has_file {
                                Theme::tint_blue_border()
                            } else {
                                Theme::tint_mint_border()
                            })
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(Theme::TEXT_BODY))
                            .font_weight(FontWeight::BOLD)
                            .text_color(if has_file {
                                Theme::accent_blue()
                            } else {
                                Theme::accent_mint()
                            })
                            .child(if has_file { "FILE" } else { "+" }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .child(
                                div()
                                    .text_size(px(Theme::TEXT_BODY_LG))
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(Theme::text_primary())
                                    .child(if has_file {
                                        fname
                                    } else {
                                        "选择音视频文件".to_string()
                                    }),
                            )
                            .children(if has_file {
                                Some(
                                    div()
                                        .text_size(px(Theme::TEXT_SMALL))
                                        .text_color(Theme::accent_mint())
                                        .child(format!("时长：{}", dur_str)),
                                )
                            } else {
                                None
                            }),
                    )
                    .child(
                        div()
                            .h(px(Theme::CHIP_H))
                            .px(px(Theme::CHIP_PAD_X))
                            .rounded(px(Theme::RADIUS_SM))
                            .flex()
                            .items_center()
                            .bg(if has_file {
                                Theme::bg_card_hover()
                            } else {
                                Theme::accent_mint()
                            })
                            .border_1()
                            .border_color(if has_file {
                                Theme::border_strong()
                            } else {
                                Theme::tint_mint_border()
                            })
                            .text_size(px(Theme::TEXT_SMALL))
                            .font_weight(FontWeight::BOLD)
                            .text_color(if has_file {
                                Theme::text_secondary()
                            } else {
                                Theme::text_on_accent()
                            })
                            .child(if has_file { "更换" } else { "浏览" }),
                    ),
            )
            // ── 3. 识别引擎档位栅格 ──
            .child(self.render_engine_card(cx))
            // ── 4. 转写核心参数（语言 / 格式 / 润色 / 线程 / GPU）──
            .child(self.render_params_card(cx))
            // ── 5. 底部主操作 CTA 按钮 (工程级突出呈现) ──
            .child(
                primitives::btn(
                    if is_processing {
                        if batch_running {
                            "终止批量"
                        } else {
                            "终止转写"
                        }
                    } else if has_file {
                        "开始转写"
                    } else if has_batch {
                        "开始全部"
                    } else {
                        "选择文件并转写"
                    },
                    primitives::BtnSize::Lg,
                    primitives::BtnVariant::Primary,
                )
                .id("sidebar-primary-cta-btn")
                .w_full()
                .gap(px(Theme::SPACE_2))
                .bg(if is_processing {
                    Theme::accent_red_strong()
                } else if can_act {
                    Theme::accent_mint()
                } else {
                    Theme::bg_disabled()
                })
                .border_1()
                .border_color(if is_processing {
                    Theme::tint_red_border()
                } else if can_act {
                    Theme::tint_mint_border()
                } else {
                    Theme::border_strong()
                })
                .text_color(if is_processing {
                    // 转写中按钮是深红实心块，文字恒白
                    Theme::text_on_saturated()
                } else if can_act {
                    Theme::text_on_accent()
                } else {
                    Theme::text_muted()
                })
                .hover(move |s| {
                    if can_act || is_processing {
                        s.opacity(0.9)
                    } else {
                        s
                    }
                })
                .on_click(cx.listener(|this, _, _, cx| {
                    let processing = matches!(this.state.status, ProcessStatus::Processing { .. });
                    if processing {
                        // 真正终止：标记取消请求并强杀识别子进程，事件回传后由收尾逻辑复位状态。
                        // 批量模式下同时停掉续跑，避免杀完当前项又自动开下一个。
                        if this.state.batch_running {
                            this.cancel_batch_queue(cx);
                        } else {
                            this.request_cancel_processing(cx);
                        }
                    } else if this.state.transcribe_file.is_some() {
                        this.start_processing(cx);
                    } else if !this.state.batch_queue.is_empty() {
                        this.start_batch_queue(cx);
                    } else {
                        this.choose_file(cx);
                    }
                })),
            )
    }

    /// 「字幕翻译」摘要卡：当前模式 / 端点 / 模型 + 一个跳转按钮。
    ///
    /// 为什么只做摘要不做控件：这一栏是 350px 的窄抽屉（[`Theme::DRAWER_W`]），
    /// 而在线翻译要填基址、密钥、模型并拉取模型列表，塞进来会把「转写参数」挤成
    /// 一屏半。真正的配置都在性能设置页的翻译卡上（含「拉取模型列表」），
    /// 这里只解决用户实测的那个坑：**配置项在界面上根本没有入口**，
    /// 之前只能手改 `config.toml`。摘要 + 一次点击即到配置处。
    #[allow(dead_code)]
    fn render_translate_summary_card(&mut self, cx: &mut Context<Self>) -> Div {
        let translate = &self.state.config.translate;
        let is_online = translate.is_online();
        let mode_label = if is_online {
            "在线 API"
        } else {
            "本地 Qwen"
        };
        // 端点只显示到主机级别：密钥与完整路径不该在这一栏多露一次
        let endpoint = if is_online {
            let url = translate.resolved_endpoint();
            match url
                .split("://")
                .nth(1)
                .and_then(|rest| rest.split('/').next())
            {
                Some(host) if !host.is_empty() => host.to_string(),
                _ => url,
            }
        } else {
            "本地推理，无需网络".to_string()
        };
        let model = if is_online {
            let model = translate.api_model.trim();
            if model.is_empty() {
                "未填写模型名".to_string()
            } else {
                model.to_string()
            }
        } else {
            String::new()
        };

        let mut card = primitives::card_sm()
            .gap(px(Theme::SPACE_2))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(primitives::section_title("字幕翻译"))
                    .child(primitives::badge(mode_label)),
            )
            .child(
                div()
                    .text_size(px(Theme::TEXT_SMALL))
                    .text_color(Theme::text_muted())
                    .truncate()
                    .child(endpoint),
            );
        if !model.is_empty() {
            card = card.child(
                div()
                    .text_size(px(Theme::TEXT_SMALL))
                    .text_color(Theme::text_secondary())
                    .truncate()
                    .child(model),
            );
        }
        card.child(
            primitives::chip_clickable("在性能设置中配置", false, false)
                .id("sidebar-translate-config-btn")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.state.active_tab = WorkspaceTab::Performance;
                    cx.notify();
                })),
        )
    }

    /// 「识别引擎」卡片：五档全部列出，没下载过的档位也可见（灰态 + 「需下载」徽标）。
    ///
    /// 老逻辑只列已下载档位，还会在**渲染过程中**把用户选中的档位悄悄改成「第一个
    /// 已就位」的那档——用户只是点开抽屉看一眼，配置就被写脏了。现在改成如实显示
    /// 「需下载」并在行内给下载入口：真正拦住开始转写的是开工守卫
    /// （`MainWindow::missing_engine_reason`），一句明确提示比偷偷换档可操作得多。
    fn render_engine_card(&mut self, cx: &mut Context<Self>) -> Div {
        let open = how_to_choose_open();
        let mut cards = div().flex().flex_col().gap(px(Theme::SPACE_1_5));
        for (index, (tier, tagline)) in TIER_TAGLINES.into_iter().enumerate() {
            cards = cards.child(self.tier_card(index, tier, tagline, cx));
        }

        primitives::card_sm()
            .gap(px(Theme::SPACE_2))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(primitives::section_title("识别引擎"))
                    // 「怎么选？」只放三行结论，不解释原理：抽屉宽 350px，
                    // 用户在这里要的是「选哪个」，不是「为什么」。
                    .child(
                        div()
                            .id("engine-how-to-choose")
                            .flex()
                            .items_center()
                            .gap(px(Theme::SPACE_1))
                            .text_size(px(Theme::TEXT_SMALL))
                            .text_color(Theme::text_secondary())
                            .cursor_pointer()
                            .hover(|s| s.text_color(Theme::accent_blue()))
                            .on_click(cx.listener(|_, _, _, cx| {
                                toggle_how_to_choose();
                                cx.notify();
                            }))
                            .child("怎么选？")
                            .child(if open { "▲" } else { "▼" }),
                    ),
            )
            .children(open.then(|| {
                div()
                    .flex()
                    .flex_col()
                    .gap(px(Theme::SPACE_1))
                    .p(px(Theme::SPACE_2))
                    .rounded(px(Theme::RADIUS_MD))
                    .bg(Theme::bg_inset())
                    .children(HOW_TO_CHOOSE_LINES.map(|line| {
                        div()
                            .text_size(px(Theme::TEXT_SMALL))
                            .text_color(Theme::text_secondary())
                            .child(line)
                    }))
            }))
            .child(cards)
    }

    /// 单个档位卡片：名字 + 定位句 + 体积 + 状态徽标，选中 / 未下载 / 下载中各有样式。
    ///
    /// 点击整卡 = 选中该档位（语义与老胶囊一致：写配置 + 把「模型与组件」页切到
    /// 对应分栏）；未下载时行内多一个动作按钮，点它不会顺带改档位。
    fn tier_card(
        &mut self,
        index: usize,
        tier: WhisperModelTier,
        tagline: &'static str,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let is_sel = self.state.whisper_model_tier == tier;
        // 「是否就位」一律取 `installed_path()`（= 引擎真正会加载的文件，含档位内
        // 量化回退），而不是按条目登记的精确文件名判定：后者会在「Turbo 用 Q8 兜底」
        // 时判为未下载，与「点开始转写其实跑得起来」自相矛盾。
        let present = tier.installed_path().is_some();
        let downloading = tier_downloading(tier, self.state.download_current.as_deref());
        let busy = self.state.is_downloading;
        let size_text = tier_size_text(tier);

        let mut head = div().flex().items_center().gap(px(Theme::SPACE_2)).child(
            div()
                .text_size(px(Theme::TEXT_BODY_LG))
                .font_weight(if is_sel {
                    FontWeight::BOLD
                } else {
                    FontWeight::MEDIUM
                })
                .text_color(if is_sel {
                    Theme::accent_blue()
                } else {
                    Theme::text_primary()
                })
                .child(tier.label()),
        );
        if is_sel {
            // 「当前」只表示「配置里选的是它」，与「下没下过」分开显示：
            // 用户因此能看懂「我选的这档还没下」，而不会以为选择没生效。
            head = head.child(primitives::badge_accent("当前"));
        }
        head = head.child(div().flex_1()).child(if present {
            primitives::badge(tier_status_text(present, downloading))
        } else if downloading {
            primitives::badge_accent(tier_status_text(present, downloading))
        } else {
            primitives::badge_danger(tier_status_text(present, downloading))
        });

        // 行内下载入口：Whisper 各档是单文件条目，就地能下；SenseVoice 由三个
        // 条目组成，按钮改成引导到「模型与组件」，避免只下了一半文件还显示就位。
        if !present && !downloading && !busy {
            match tier.download_item_id() {
                Some(item_id) => {
                    head = head.child(
                        primitives::mini_btn("下载", true)
                            .id(("engine-tier-dl", index))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                // 阻止冒泡：否则点「下载」会顺带触发整卡的「选中该档位」。
                                cx.stop_propagation();
                                this.start_model_download(item_id, cx);
                            })),
                    );
                }
                None => {
                    head = head.child(
                        primitives::mini_btn("去下载", true)
                            .id(("engine-tier-guide", index))
                            .on_click(cx.listener(|this, _, _, cx| {
                                cx.stop_propagation();
                                this.model_manager_tab = crate::ui::ModelManagerTab::SenseVoice;
                                this.state.active_tab = WorkspaceTab::Performance;
                                cx.notify();
                            })),
                    );
                }
            }
        }

        let body = div()
            .flex()
            .flex_col()
            .gap(px(Theme::SPACE_0_5))
            .child(head)
            .child(
                div()
                    .text_size(px(Theme::TEXT_SMALL))
                    .text_color(Theme::text_secondary())
                    .child(tagline),
            )
            .child(
                div()
                    .text_size(px(Theme::TEXT_CAPTION))
                    .text_color(Theme::text_muted())
                    .child(size_text),
            );

        div()
            .id(("engine-tier-card", index))
            .w_full()
            .px(px(Theme::SPACE_2_5))
            .py(px(Theme::SPACE_2))
            .rounded(px(Theme::RADIUS_LG))
            .border_1()
            .border_color(if is_sel {
                Theme::tint_blue_border()
            } else {
                Theme::border()
            })
            .bg(if is_sel {
                Theme::tint_blue_badge()
            } else if present {
                Theme::bg_raised()
            } else {
                Theme::bg_inset()
            })
            .cursor_pointer()
            .hover(move |s| {
                if is_sel {
                    s
                } else {
                    s.bg(Theme::tint_neutral())
                        .border_color(Theme::border_light())
                }
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                this.state.set_whisper_model_tier(tier);
                this.model_manager_tab = match tier {
                    crate::app::WhisperModelTier::SenseVoice => {
                        crate::ui::ModelManagerTab::SenseVoice
                    }
                    _ => crate::ui::ModelManagerTab::Whisper,
                };
                cx.notify();
            }))
            .child(body)
    }

    /// 「转写参数」卡片：识别语言 / 输出格式 / 标点润色 / 转写线程
    fn render_params_card(&mut self, cx: &mut Context<Self>) -> Div {
        let lang_sel = match self.state.language.as_str() {
            "zh" => "zh",
            "ja" => "ja",
            "en" => "en",
            "ko" => "ko",
            "yue" => "yue",
            _ => "auto",
        };
        // 兜底回落到 "srt" 之前，必须把新增的专业格式也列出来——否则 output_format
        // 明明是 json/ttml/ttal，界面上却高亮 SRT，用户以为设置没生效。
        let fmt_sel = match self.state.output_format.as_str() {
            "vtt" => "vtt",
            "ass" => "ass",
            "txt" => "txt",
            "json" => "json",
            "ttml" => "ttml",
            "ttal" => "ttal",
            _ => "srt",
        };
        // 逐词卡拉OK开关（落盘键：`[subtitle_style] karaoke_word_highlight`）。
        let karaoke_sel = if self.state.config.subtitle_style.karaoke_word_highlight {
            "on"
        } else {
            "off"
        };
        let polish_sel = if !self.state.enable_polish {
            "off"
        } else if self.state.polish_mode == PolishMode::PuncFast {
            "punc"
        } else {
            "qwen"
        };
        let thread_sel = self.state.whisper_threads.to_string();
        // 线程候选项与性能页同源：上限取本机逻辑核心数，并并入配置当前值
        let thread_options: Vec<(String, String)> = thread_presets(
            self.state.whisper_threads,
            crate::app::AppState::logical_cores(),
        )
        .into_iter()
        .map(|n| (n.to_string(), format!("{n} 线程")))
        .collect();
        // 说话人数量：选项与收敛区间都以 diarization::MAX_SPEAKERS 为准，
        // 不在这里另写一份字面量。引擎把人数收敛到 2..=MAX_SPEAKERS，
        // UI 若还留着旧的固定 4，用户选了也白选。
        let speaker_options = speaker_options();
        let speaker_sel = if !self.state.config.pipeline.enable_diarization {
            "off".to_string()
        } else {
            self.state
                .config
                .pipeline
                .speaker_count
                .clamp(2, MAX_SPEAKERS)
                .to_string()
        };
        let gpu_sel = self.state.gpu_mode.clone();
        let gpu_limit_sel = self.state.config.gpu.effective_gpu_limit().to_string();

        primitives::card_sm()
            .gap(px(Theme::SPACE_2))
            .child(self.param_group(
                "GPU 占用策略",
                vec![
                    ("full", "全速 GPU"),
                    ("balanced", "GPU 让路"),
                    ("eco", "低占用 GPU"),
                    ("cpu", "纯 CPU"),
                ],
                &gpu_sel,
                |this, sel, cx| {
                    this.state.gpu_mode = sel.to_string();
                    this.state.config.gpu = crate::utils::config::GpuConfig::from_mode(sel);
                    // 让路开关是**进程级全局量**，只在启动时下发一次。用户改档位后
                    // 必须立即重下发，否则「保存了但本进程仍按旧值跑」，要重启才生效。
                    crate::engines::media_pipeline::set_yield_to_desktop(
                        this.state.config.gpu.yield_to_desktop,
                    );
                    // `from_mode` 会整段重写 gpu_limit_percent（如 eco 档给 60），
                    // 因此切档位也要重下发一次，否则上限沿用上一个档位残留的值。
                    this.state
                        .pipeline
                        .set_gpu_limit_percent(this.state.config.gpu.effective_gpu_limit());
                    this.state.persist_config();
                    cx.notify();
                },
                cx,
            ))
            .child(if gpu_sel == "eco" {
                self.param_group(
                    "GPU 占用上限",
                    vec![("40", "40%"), ("60", "60%"), ("80", "80%")],
                    &gpu_limit_sel,
                    |this, sel, cx| {
                        this.state.config.gpu.gpu_limit_percent = sel.parse::<u32>().unwrap_or(60);
                        // 立即下发给引擎：GPU 限速是「子进程按占空比挂起/恢复」，
                        // 只存配置的话本进程仍按旧档位跑，用户要重启才看到效果。
                        this.state
                            .pipeline
                            .set_gpu_limit_percent(this.state.config.gpu.effective_gpu_limit());
                        this.state.persist_config();
                        cx.notify();
                    },
                    cx,
                )
            } else {
                div()
            })
            .child(self.param_group(
                "识别语言",
                vec![
                    ("auto", "自动"),
                    ("zh", "中文"),
                    ("ja", "日语"),
                    ("en", "英语"),
                    ("ko", "韩语"),
                    ("yue", "粤语"),
                ],
                lang_sel,
                |this, sel, cx| {
                    this.state.language = sel.to_string();
                    // 与同卡片其它项一致：状态 + config.toml 双写。此前只改 state，
                    // 重启后 `AppState::with_hardware` 从配置读回的仍是旧语言。
                    this.state.config.pipeline.language = sel.to_string();
                    this.state.persist_config();
                    cx.notify();
                },
                cx,
            ))
            .child(self.param_group(
                "字幕输出格式",
                vec![
                    ("srt", "SRT"),
                    ("vtt", "VTT"),
                    ("ass", "ASS"),
                    ("txt", "TXT"),
                    ("json", "JSON"),
                    ("ttml", "EBU-TT-D"),
                    ("ttal", "Netflix TTAL"),
                ],
                fmt_sel,
                |this, sel, cx| {
                    this.state.output_format = sel.to_string();
                    // 同上：不落盘的话，重启后配置里的旧格式会把这次选择覆盖掉
                    this.state.config.pipeline.output_format = sel.to_string();
                    this.state.persist_config();
                    cx.notify();
                },
                cx,
            ))
            // 逐词卡拉OK高亮：只影响 ASS 原文行的逐词点亮（`{\kfN}`），放在「字幕输出格式」
            // 之后——格式选 ASS 才有它，两个控件是同一件事的两面。默认开启；没有词级数据
            // 的老工程一个标签都不会插，导出结果与关闭时逐字节一致。
            .child(self.param_group(
                "逐词卡拉OK高亮",
                vec![("on", "开启"), ("off", "关闭")],
                karaoke_sel,
                |this, sel, cx| {
                    this.state.config.subtitle_style.karaoke_word_highlight = sel == "on";
                    // 与样式面板同源：样式类改动一律走 save_subtitle_style（落盘 + 同步给管线），
                    // 否则重启后配置里的旧值会把这次选择覆盖掉。
                    this.state.save_subtitle_style();
                    cx.notify();
                },
                cx,
            ))
            .child(self.param_group(
                "标点与润色",
                {
                    // SenseVoice 原生自带标点与 ITN，管线会跳过 CT-Punc 阶段。此时
                    // 「极速标点」是一个**无操作**档位，必须就地写明，否则用户选了
                    // 它却什么都没发生，还以为程序没记住自己的选择。
                    if self.state.punc_polish_is_noop() {
                        vec![
                            ("off", "关闭"),
                            ("punc", "极速标点（SenseVoice 原生自带）"),
                            ("qwen", "Qwen 润色"),
                        ]
                    } else {
                        vec![("off", "关闭"), ("punc", "极速标点"), ("qwen", "Qwen 润色")]
                    }
                },
                polish_sel,
                |this, sel, cx| {
                    // 统一走 `AppState::set_polish`：状态 + config.toml 双写。
                    // 此前这里只改内存，重启后润色开关与档位都会被配置里的旧值覆盖。
                    // 「关闭」时沿用当前档位，只把总开关置假——否则用户关一下再开，
                    // 引擎会悄悄从 Qwen 变回 CT-Punc。
                    let mode = this.state.polish_mode;
                    match sel {
                        "punc" => this.state.set_polish(true, PolishMode::PuncFast),
                        "qwen" => this.state.set_polish(true, PolishMode::QwenDeep),
                        _ => this.state.set_polish(false, mode),
                    }
                    cx.notify();
                },
                cx,
            ))
            .child(self.param_group(
                "说话人分离",
                speaker_options,
                &speaker_sel,
                |this, sel, cx| {
                    match sel {
                        "off" => this.state.config.pipeline.enable_diarization = false,
                        n => {
                            this.state.config.pipeline.enable_diarization = true;
                            this.state.config.pipeline.speaker_count =
                                n.parse::<u32>().unwrap_or(2);
                        }
                    }
                    this.state.persist_config();
                    cx.notify();
                },
                cx,
            ))
            .child(self.param_group(
                "转写线程",
                thread_options,
                &thread_sel,
                |this, sel, cx| {
                    let v = sel.parse::<u32>().unwrap_or(8);
                    if this.state.whisper_threads != v {
                        this.state.whisper_threads = v;
                        // 与性能页滑条同一落盘口径（见 views/performance.rs）
                        this.state.config.pipeline.whisper_threads = v;
                        this.state.persist_config();
                        cx.notify();
                    }
                },
                cx,
            ))
            // 热词 / 自定义提示词：识别相关的最后一格（人名、品牌、专业术语的音译纠正）。
            // 追加在末尾、不挪动任何既有条目：抽屉布局是用户肌肉记忆的一部分。
            .child(self.render_hotwords_row(cx))
    }

    /// 参数分组：小标题 + 分段选择器行
    fn param_group<K, L>(
        &mut self,
        label: &'static str,
        options: Vec<(K, L)>,
        selected: &str,
        on_select: impl Fn(&mut Self, &str, &mut Context<Self>) + Copy + 'static,
        cx: &mut Context<Self>,
    ) -> Div
    where
        K: Into<SharedString> + Clone + 'static,
        L: Into<SharedString>,
    {
        div()
            .flex()
            .flex_col()
            .gap(px(Theme::SPACE_1))
            .child(primitives::field_label(label))
            .child(self.pill_row(options, selected, on_select, cx))
    }

    /// 通用分段选择器：一行等宽胶囊，单击切换（多于4项时自适应换行，避免溢出重叠）
    fn pill_row<K, L>(
        &mut self,
        options: Vec<(K, L)>,
        selected: &str,
        on_select: impl Fn(&mut Self, &str, &mut Context<Self>) + Copy + 'static,
        cx: &mut Context<Self>,
    ) -> Div
    where
        K: Into<SharedString> + Clone + 'static,
        L: Into<SharedString>,
    {
        let is_wrap = options.len() > 4;
        let mut row = div().w_full().flex().gap(px(Theme::SPACE_1_5));
        if is_wrap {
            row = row.flex_wrap();
        }
        for (key, label) in options {
            let key: SharedString = key.into();
            let is_sel = key.as_str() == selected;
            let pill_id = key.clone();
            row = row.child(
                primitives::segmented(label, is_sel, !is_wrap)
                    .id(pill_id)
                    .on_click(cx.listener(move |this, _, _, cx| on_select(this, key.as_str(), cx))),
            );
        }
        row
    }

    /// 「热词 / 自定义提示词」输入行（落盘键：`[pipeline] asr_hotwords`）。
    ///
    /// # 外观状态为什么自己维护
    /// 这条渲染链（`render_sidebar` → 本函数）拿不到 `Window`，问不了 GPUI「这个框现在有焦点吗」。
    /// 本窗其它输入框都靠 `MainWindow::render` 每帧从窗口焦点同步一份 bool（见 `src/ui/mod.rs`
    /// 的 `focused_api_field`），而那个同步点在禁改文件里。于是改成事件驱动：点击置真；
    /// 失焦订阅与键盘拦截器里置假，两处都会 `cx.notify()`。
    ///
    /// # 键盘为什么走全局拦截器
    /// 本窗的裸键动作（空格 = 播放/暂停、Delete/Backspace = 删除当前句、Ctrl+Z = 撤销字幕）
    /// 用 `MainWindow::any_input_focused` 判定是否放行，而那份焦点句柄清单写死在
    /// `src/ui/mod.rs`（禁改）。本框不在清单里，若只挂 `on_key_down`：空格会被动作系统吃掉
    /// （既打不出空格，又顺带切换播放），Delete 还会删掉一句字幕。`App::intercept_keystrokes`
    /// 在动作分发**之前**执行，正好只接管这几个冲突键；其余按键照旧走元素自己的 `on_key_down`，
    /// 与同窗其它输入框共用 `crate::ui::apply_line_edit` 那套最小编辑集合。
    fn render_hotwords_row(&mut self, cx: &mut Context<Self>) -> Div {
        let focus = match HOTWORDS_INPUT.with(|s| s.borrow().focus.clone()) {
            Some(handle) => handle,
            None => {
                let created = cx.focus_handle();
                HOTWORDS_INPUT.with(|s| s.borrow_mut().focus = Some(created.clone()));
                created
            }
        };
        let (focused, cursor) = HOTWORDS_INPUT.with(|s| {
            let s = s.borrow();
            (s.focused, s.cursor)
        });
        let raw = self.state.config.pipeline.asr_hotwords.clone();
        let char_count = raw.chars().count();
        let (before, after) = hotwords_display_window(&raw, cursor.min(char_count));
        let has_content = char_count > 0;
        let click_focus = focus.clone();

        let input_box = primitives::text_input(focused, 120.0)
            .id("asr-hotwords-input")
            .track_focus(&focus)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    window.focus(&click_focus);
                    HOTWORDS_INPUT.with(|s| {
                        let mut s = s.borrow_mut();
                        s.focused = true;
                        // 点进来时光标落到末尾：这个框的主用法是「在末尾补一个词」
                        s.cursor = this.state.config.pipeline.asr_hotwords.chars().count();
                    });
                    this.ensure_hotwords_hooks(window, cx);
                    cx.notify();
                }),
            )
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _window, cx| {
                // 能走到这里的按键都是「动作系统不争」的那些：空格 / 退格 / Delete / Ctrl+Z 系
                // 已在拦截器里被接管，剩下的一律共用本窗其它输入框那套编辑核，不在这里再写一份。
                let mut cursor = HOTWORDS_INPUT.with(|s| s.borrow().cursor);
                if crate::ui::apply_line_edit(
                    &mut this.state.config.pipeline.asr_hotwords,
                    &mut cursor,
                    event,
                    cx,
                ) {
                    HOTWORDS_INPUT.with(|s| s.borrow_mut().cursor = cursor);
                    // 逐键落盘：转写侧每次拼命令行都现读 config.toml，「改完即时生效」
                    // 就落在这一次写入上（与同卡片其它设置项同一套 persist 口径）。
                    this.state.persist_config();
                    cx.notify();
                }
            }))
            .child(if focused {
                div()
                    .flex()
                    .items_center()
                    .text_size(px(Theme::TEXT_BODY))
                    .text_color(Theme::text_primary())
                    .child(before)
                    .child(div().text_color(Theme::accent_blue()).child("▌"))
                    .child(after)
            } else if char_count == 0 {
                div()
                    .text_size(px(Theme::TEXT_SMALL))
                    .text_color(Theme::text_muted())
                    .truncate()
                    .child("例如：张三, 李四, OpenAI, 量子纠缠")
            } else {
                div()
                    .text_size(px(Theme::TEXT_BODY))
                    .text_color(Theme::text_primary())
                    .truncate()
                    .child(raw)
            });

        div()
            .flex()
            .flex_col()
            .gap(px(Theme::SPACE_1))
            .child(primitives::field_label("热词提示词"))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(Theme::SPACE_1_5))
                    .w_full()
                    .child(div().flex_1().min_w(px(0.0)).child(input_box))
                    .children(has_content.then(|| {
                        primitives::chip_clickable("清空", false, false)
                            .id("hotwords-clear")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.state.config.pipeline.asr_hotwords.clear();
                                HOTWORDS_INPUT.with(|s| s.borrow_mut().cursor = 0);
                                this.state.persist_config();
                                cx.notify();
                            }))
                    }))
                    .child(
                        primitives::chip_clickable("粘贴", false, false)
                            .id("hotwords-paste")
                            .on_click(cx.listener(|this, _, _, cx| {
                                let Some(text) = cx.read_from_clipboard().and_then(|it| it.text())
                                else {
                                    return;
                                };
                                // 多行文本折成一行：热词是「一行词表」，换行在提示词里没有语义
                                //（whisper.rs 侧还会把控制字符再折一遍，这里先折成逗号更直观）。
                                let flat = text.trim().replace(['\r', '\n'], "，");
                                if flat.is_empty() {
                                    return;
                                }
                                let new_cursor = {
                                    let buffer = &mut this.state.config.pipeline.asr_hotwords;
                                    if !buffer.is_empty()
                                        && !buffer.ends_with(',')
                                        && !buffer.ends_with('，')
                                    {
                                        buffer.push('，');
                                    }
                                    buffer.push_str(&flat);
                                    buffer.chars().count()
                                };
                                HOTWORDS_INPUT.with(|s| s.borrow_mut().cursor = new_cursor);
                                this.state.persist_config();
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .text_size(px(Theme::TEXT_CAPTION))
                    .text_color(Theme::text_muted())
                    // 浅显说明：只讲「填什么、怎么分隔、会发生什么」。末尾一句写明仅 Whisper 生效——
                    // SenseVoice 走 sherpa-onnx 的单次前向（CTC 类），没有 initial prompt /
                    // 解码偏置这类接口，硬塞参数只会被 runner 忽略，不如讲清楚。
                    .child(
                        "填人名、品牌、专业术语，用逗号分隔；识别时会提示模型优先按这些写法输出。\
                         仅 Whisper 引擎生效（SenseVoice 无提示词接口）。",
                    ),
            )
    }

    /// 热词输入框的一次性全局钩子：键盘拦截 + 失焦订阅。
    ///
    /// 挂在「第一次点击输入框」那一刻，而不是每帧渲染时：`Subscription` 一 drop 就注销，
    /// 必须留一份活到窗口结束，而每帧挂一份会越积越多。点击时手上同时有 `Window` 与 `Context`，
    /// 正好是这两类钩子都需要的时刻（渲染路径没有 `Window`）。
    fn ensure_hotwords_hooks(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (handle, need_keys, need_blur) = HOTWORDS_INPUT.with(|s| {
            let s = s.borrow();
            (s.focus.clone(), s.key_hook.is_none(), s.blur_hook.is_none())
        });
        let Some(handle) = handle else {
            return;
        };

        if need_keys {
            let weak = cx.weak_entity();
            let focus = handle.clone();
            let sub = cx.intercept_keystrokes(move |event: &KeystrokeEvent, window, cx| {
                let focused_now = focus.is_focused(window);
                // 外观标记在这里自愈：渲染时拿不到 `Window`，真实焦点只有拦截器（每次按键）
                // 与失焦订阅（每次失焦）看得见。
                let stale = HOTWORDS_INPUT.with(|s| s.borrow().focused != focused_now);
                if stale {
                    HOTWORDS_INPUT.with(|s| s.borrow_mut().focused = focused_now);
                    let _ = weak.update(cx, |_, cx| cx.notify());
                }
                if !focused_now {
                    return; // 焦点不在热词框：一个按键都不碰
                }
                // 只接管会与全局裸键动作打架的那几个键；返回 true 才 `stop_propagation()`，
                // 于是「空格切播放 / Delete 删字幕 / Ctrl+Z 撤销字幕」都不会发生。
                let consumed = weak
                    .update(cx, |this, cx| {
                        this.intercept_hotwords_keystroke(&event.keystroke, cx)
                    })
                    .unwrap_or(false);
                if consumed {
                    cx.stop_propagation();
                }
            });
            HOTWORDS_INPUT.with(|s| s.borrow_mut().key_hook = Some(sub));
        }

        if need_blur {
            let weak = cx.weak_entity();
            let sub = window.on_focus_out(&handle, cx, move |_event, _window, cx| {
                HOTWORDS_INPUT.with(|s| s.borrow_mut().focused = false);
                let _ = weak.update(cx, |_, cx| cx.notify());
            });
            HOTWORDS_INPUT.with(|s| s.borrow_mut().blur_hook = Some(sub));
        }
    }

    /// 冲突按键的就地编辑：返回 `true` 表示这个键由热词输入框接管。
    ///
    /// 接管范围刻意只有「动作系统会误伤」的几类：空格（裸键 = 播放/暂停，且键名是多字符
    /// 所以 `apply_line_edit` 插不进去）、退格 / Delete（裸键 = 删除当前句）、Ctrl+Z 系
    /// （裸键 = 撤销**字幕**，不是撤销这段文本）。其余按键一律放行给元素自己的 `on_key_down`，
    /// 编辑行为与同窗其它输入框保持一致。
    fn intercept_hotwords_keystroke(
        &mut self,
        keystroke: &Keystroke,
        cx: &mut Context<Self>,
    ) -> bool {
        let key = keystroke.key.to_ascii_lowercase();
        if keystroke.modifiers.control && matches!(key.as_str(), "z" | "y") {
            // 本框没有撤销栈：吞掉即可，绝不能让动作系统去改字幕。
            return true;
        }
        let mut cursor = HOTWORDS_INPUT.with(|s| s.borrow().cursor);
        let consumed = apply_hotwords_bare_key(
            &mut self.state.config.pipeline.asr_hotwords,
            &mut cursor,
            &key,
        );
        if consumed {
            HOTWORDS_INPUT.with(|s| s.borrow_mut().cursor = cursor);
            self.state.persist_config();
            cx.notify();
        }
        consumed
    }

    /// 渲染硬件与模型资源监控对比卡片 (CPU / 内存实时对比)
    #[allow(dead_code)]
    pub(crate) fn render_hardware_monitor_card(&self, _cx: &mut Context<Self>) -> impl IntoElement {
        let m = &self.state.metrics;
        let sys_cpu_pct = m.sys_cpu.clamp(0.0, 100.0);
        let proc_cpu_pct = m.proc_cpu.clamp(0.0, 100.0);

        let sys_mem_gb = m.sys_mem_used as f64 / (1024.0 * 1024.0 * 1024.0);
        let total_mem_gb = (m.sys_mem_total as f64 / (1024.0 * 1024.0 * 1024.0)).max(1.0);
        let sys_mem_pct = ((sys_mem_gb / total_mem_gb) * 100.0).clamp(0.0, 100.0) as f32;

        let proc_mem_gb = m.proc_mem as f64 / (1024.0 * 1024.0 * 1024.0);
        let proc_mem_pct = ((proc_mem_gb / total_mem_gb) * 100.0).clamp(0.0, 100.0) as f32;

        primitives::card_sm()
            .id("hardware-monitor-card")
            // 标头行：标题 + 状态胶囊
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(primitives::field_label("系统监控"))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(primitives::stat_dot_sm(if m.is_model_running {
                                Theme::accent_mint()
                            } else {
                                Theme::text_muted()
                            }))
                            .child(
                                div()
                                    .text_size(px(Theme::TEXT_CAPTION))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(if m.is_model_running {
                                        Theme::accent_mint()
                                    } else {
                                        Theme::text_secondary()
                                    })
                                    .child(m.proc_name.clone()),
                            ),
                    ),
            )
            // 1. CPU 对比条
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .text_size(px(Theme::TEXT_SMALL))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .child(div().text_color(Theme::text_muted()).child(
                                        if m.is_model_running {
                                            "CPU（大模型）："
                                        } else {
                                            "CPU（应用待机）："
                                        },
                                    ))
                                    .child(
                                        div()
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(Theme::accent_mint())
                                            .child(format!("{:.1}%", proc_cpu_pct)),
                                    ),
                            )
                            .child(
                                div()
                                    .text_color(Theme::text_muted())
                                    .child(format!("系统：{:.1}%", sys_cpu_pct)),
                            ),
                    )
                    // 双层条
                    .child(primitives::meter_bar(
                        sys_cpu_pct / 100.0,
                        proc_cpu_pct / 100.0,
                        Theme::accent_mint(),
                    )),
            )
            // 2. 内存对比条
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .text_size(px(Theme::TEXT_SMALL))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .child(div().text_color(Theme::text_muted()).child(
                                        if m.is_model_running {
                                            "内存（大模型）："
                                        } else {
                                            "内存（应用待机）："
                                        },
                                    ))
                                    .child(
                                        div()
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(Theme::accent_blue())
                                            .child(crate::app::ResourceMetrics::format_bytes(
                                                m.proc_mem,
                                            )),
                                    ),
                            )
                            .child(
                                div()
                                    .text_color(Theme::text_muted())
                                    .child(format!("{:.1} / {:.0} GB", sys_mem_gb, total_mem_gb)),
                            ),
                    )
                    // 双层内存条
                    .child(primitives::meter_bar(
                        sys_mem_pct / 100.0,
                        proc_mem_pct / 100.0,
                        Theme::accent_blue(),
                    )),
            )
    }
}

#[cfg(test)]
mod tests {
    // 这里刻意不用 `use super::*`：父模块的 `use gpui::*` 会把 gpui 自带的
    // `test` 属性宏带进来，遮蔽内建的 `#[test]`，导致展开递归超限。
    use super::{
        apply_hotwords_bare_key, bundle_size_text, hotwords_display_window, speaker_options,
        tier_downloading, tier_size_text, tier_status_text, HOTWORDS_VIEW_CHARS, TIER_TAGLINES,
    };
    use crate::app::WhisperModelTier;
    use crate::engines::MAX_SPEAKERS;

    /// 回归测试：2026-09-28 两次「点开视频转写就闪退」的根因，就是这里少了一项——
    /// 选项表写成固定的 4 项（关闭 + 2/3/4 人），而当时那句断言要求 5 项，
    /// 于是每次渲染右侧配置面板都 panic。
    /// 这里把「选项必须完整覆盖引擎的 2..=MAX_SPEAKERS」钉住。
    #[test]
    fn speaker_options_cover_engine_range() {
        let options = speaker_options();

        assert_eq!(
            options.len(),
            MAX_SPEAKERS as usize,
            "选项应为「关闭」+ 2..=MAX_SPEAKERS，共 MAX_SPEAKERS 项"
        );
        assert_eq!(options[0], ("off", "关闭"));

        for (i, (key, label)) in options[1..].iter().enumerate() {
            let count = i + 2;
            assert_eq!(*key, count.to_string(), "第 {i} 项的选项值应为 {count}");
            assert_eq!(*label, format!("{count} 人"), "第 {i} 项的显示名不匹配");
        }

        // 配置里存的人数必须能在选项表里找到，否则胶囊会全部处于未选中态
        let selected = 2u32.clamp(2, MAX_SPEAKERS).to_string();
        assert!(options.iter().any(|(key, _)| *key == selected));
    }

    /// 五档卡片必须齐：抽屉里少一档，用户就再也选不到那个引擎（老逻辑只列已下载档位，
    /// 未下载的档位会整块消失，这正是「想选 Turbo 却找不到」的来源）。
    #[test]
    fn tier_taglines_cover_every_tier_once() {
        let mut seen: Vec<&str> = Vec::new();
        for (tier, tagline) in TIER_TAGLINES.iter() {
            assert!(!tagline.trim().is_empty(), "{tier:?} 缺定位句");
            assert!(
                !seen.contains(tagline),
                "{tier:?} 的定位句与前面的档位重复：{tagline}"
            );
            seen.push(tagline);
        }

        let covered: Vec<WhisperModelTier> = TIER_TAGLINES.iter().map(|(t, _)| *t).collect();
        for tier in WhisperModelTier::WHISPER_TIERS {
            assert!(covered.contains(&tier), "抽屉缺少 {tier:?} 的卡片");
        }
        assert!(covered.contains(&WhisperModelTier::SenseVoice));
        assert_eq!(covered.len(), WhisperModelTier::WHISPER_TIERS.len() + 1);
    }

    /// 体积文案：正常条目要给得出数字，查不到的条目要老实说「体积未知」而不是空白。
    #[test]
    fn tier_size_text_reports_known_sizes_and_falls_back() {
        assert_eq!(bundle_size_text(&[]), "体积未知");
        assert_eq!(bundle_size_text(&["no-such-item"]), "体积未知");

        let base = bundle_size_text(&["whisper-base"]);
        assert!(base.starts_with("约 "), "正常条目应带「约」前缀：{base}");
        assert!(base.contains("MB"), "Base 档应为 MB 级：{base}");

        // SenseVoice = 模型 + 词表 + VAD，比单看模型条目更大：只报模型会低估下载量。
        let bundle = tier_size_text(WhisperModelTier::SenseVoice);
        assert_ne!(bundle, "体积未知");
        assert_ne!(
            bundle,
            bundle_size_text(&["sensevoice-model"]),
            "SenseVoice 体积必须是三条目合计"
        );
    }

    /// 徽标优先级：正在下载的档位不能说成「已下载」，否则用户以为已经能用了。
    #[test]
    fn tier_status_text_prefers_downloading_over_present() {
        assert_eq!(tier_status_text(true, false), "已下载");
        assert_eq!(tier_status_text(false, false), "需下载");
        assert_eq!(tier_status_text(false, true), "下载中…");
        assert_eq!(tier_status_text(true, true), "下载中…");
    }

    /// 下载态判定：Whisper 档按 id 精确匹配，SenseVoice 按前缀匹配三条目中的任意一个。
    #[test]
    fn tier_downloading_matches_bundle_by_prefix() {
        assert!(!tier_downloading(WhisperModelTier::Fast, None));
        assert!(tier_downloading(
            WhisperModelTier::Fast,
            Some("whisper-base")
        ));
        assert!(!tier_downloading(
            WhisperModelTier::Fast,
            Some("whisper-turbo-q5")
        ));
        assert!(tier_downloading(
            WhisperModelTier::SenseVoice,
            Some("sensevoice-tokens")
        ));
        assert!(!tier_downloading(
            WhisperModelTier::SenseVoice,
            Some("whisper-base")
        ));
    }

    /// 热词框的裸键编辑核：空格必须能打出来（`apply_line_edit` 的键名判据做不到这一点），
    /// 退格删光标前一个字符、Delete 删光标后一个字符，其它键一律放行。
    #[test]
    fn hotwords_bare_keys_edit_at_cursor() {
        let mut buffer = "张三李四".to_string();
        let mut cursor = 2; // 「张」「三」之后

        // 空格：插在光标处（自绘输入框里唯一能打出空格的地方）
        assert!(apply_hotwords_bare_key(&mut buffer, &mut cursor, "space"));
        assert_eq!(buffer, "张三 李四");
        assert_eq!(cursor, 3);

        // 退格：删光标前的那个空格
        assert!(apply_hotwords_bare_key(
            &mut buffer,
            &mut cursor,
            "backspace"
        ));
        assert_eq!(buffer, "张三李四");
        assert_eq!(cursor, 2);

        // Delete：删光标后的「李」（光标不动）
        assert!(apply_hotwords_bare_key(&mut buffer, &mut cursor, "delete"));
        assert_eq!(buffer, "张三四");
        assert_eq!(cursor, 2);

        // 到边界也不 panic、不越界
        cursor = 0;
        assert!(apply_hotwords_bare_key(
            &mut buffer,
            &mut cursor,
            "backspace"
        ));
        assert_eq!(buffer, "张三四");
        cursor = buffer.chars().count();
        assert!(apply_hotwords_bare_key(&mut buffer, &mut cursor, "delete"));
        assert_eq!(buffer, "张三四");

        // 别的键不归它管（交给 `apply_line_edit`）
        assert!(!apply_hotwords_bare_key(&mut buffer, &mut cursor, "a"));
        assert!(!apply_hotwords_bare_key(&mut buffer, &mut cursor, "left"));
        assert_eq!(buffer, "张三四");
    }

    /// 光标窗口：长热词里光标必须留在可见范围内，两侧被切掉时给出省略号。
    #[test]
    fn hotwords_display_window_keeps_cursor_visible() {
        // 短文本整段可见，不加省略号
        let (before, after) = hotwords_display_window("张三,李四", 3);
        assert_eq!((before.as_str(), after.as_str()), ("张三,", "李四"));

        // 光标越界（缓冲刚被清空过）时夹到末尾，不 panic
        let (before, after) = hotwords_display_window("张三", 99);
        assert_eq!((before.as_str(), after.as_str()), ("张三", ""));

        // 长文本：光标左右各 `HOTWORDS_VIEW_CHARS` 个字符，两侧都补省略号
        let long = "人".repeat(300);
        let (before, after) = hotwords_display_window(&long, 150);
        assert_eq!(before.chars().count(), HOTWORDS_VIEW_CHARS + 1, "{before}");
        assert_eq!(after.chars().count(), HOTWORDS_VIEW_CHARS + 1, "{after}");
        assert!(before.starts_with('…') && after.ends_with('…'));

        // 光标在头部 / 尾部时只有一侧需要省略号
        let (before, after) = hotwords_display_window(&long, 0);
        assert!(!before.starts_with('…') && before.is_empty());
        assert!(after.ends_with('…'));
        let (before, after) = hotwords_display_window(&long, 300);
        assert!(before.starts_with('…') && !after.ends_with('…') && after.is_empty());
    }
}
