//! 模型与组件管理面板（首次使用引导与模型库）。
//!
//! # 架构设计
//!
//! 1. **零冗余依赖**：SenseVoice 默认模式为纯自包含 ONNX 架构，无需任何 whisper.cpp / llama.cpp；
//!    在线 API 翻译模式无需本地 Qwen / llama.cpp。
//! 2. **镜像加速**：优先直连阿里云魔搭社区（ModelScope CDN，实测 10MB/s+），自动回落 HuggingFace 镜像。
//! 3. **视觉层次优化**：核心模型与底层执行程序分流展示，系统维护工具独立沉底。

use std::collections::HashMap;
use std::path::PathBuf;

use gpui::prelude::*;
use gpui::{div, px, relative, AnyElement, FontWeight, Global, IntoElement, ParentElement, Styled};

use crate::ui::primitives;
use crate::ui::theme::Theme;
use crate::ui::{MainWindow, ModelManagerTab};
use crate::utils::model_download::human_size;
use crate::utils::ITEMS;

/// 「模型与组件」页的一个分块。
#[derive(Clone, Copy)]
struct ModelBlock {
    /// 分块标识：既是 [`is_item_needed_for_tab`] 的过滤维度，也是
    /// [`crate::ui::MainWindow::model_blocks_open`] 的下标。
    tab: ModelManagerTab,
    /// 分块标题
    title: &'static str,
    /// 一句话说明这块是干什么的
    subtitle: &'static str,
    /// 块内的分类（小标题 + 条目 id）
    categories: &'static [ModelCategory],
}

/// 分块内的一个分类。
#[derive(Clone, Copy)]
struct ModelCategory {
    title: &'static str,
    item_ids: &'static [&'static str],
}

/// 三个分块的定义，数组顺序即页面从上到下的顺序。
///
/// # 为什么合成一张数据表而不是三个 `match` 分支
///
/// 渲染流程（块头 → 分类 → 条目行）三块完全同构，差别只在数据。写成数据表后，
/// 新增一个分块只需要在这里加一项，渲染代码一行都不用动。早先这里是
/// `match current_tab { SenseVoice => ..., Whisper => ..., Common => ... }`，
/// 改一处要同步三处，正是「三个专区只能看一个」这个别扭设计的根源。
/// Whisper 语音识别可选模型（任选其一即可）的全部条目 ID
/// 「快速 / 均衡」档：日常首选，体积与速度友好。
const WHISPER_FAST_IDS: &[&str] = &[
    "whisper-turbo-q5",
    "whisper-turbo-q8",
    "whisper-medium",
    "whisper-small",
    "whisper-base",
    "whisper-tiny",
];

/// 「高精度」档：同尺寸下更准，但更慢、更大。按需自行下载。
const WHISPER_ACCURATE_IDS: &[&str] = &[
    "whisper-large-v3",
    "whisper-large-v3-q5_0",
    "whisper-large-v3-turbo-f16",
    "whisper-large-v2",
    "whisper-medium-q8",
];

/// 「英语专用」档：`.en` 模型只认英文，纯英文素材上比同尺寸多语种模型更准。
const WHISPER_ENGLISH_IDS: &[&str] = &["whisper-medium-en", "whisper-small-en"];

/// Whisper 语音识别可选模型（任选其一即可）的**全部**条目 ID。
///
/// 由上面三档拼成：界面按档位分区展示，而「是不是可选的 Whisper 模型」这类判断
/// 只需要一个全集，不必让调用方知道分区细节。
pub const WHISPER_CHOICE_MODEL_IDS: &[&str] = &[
    "whisper-turbo-q5",
    "whisper-turbo-q8",
    "whisper-large-v3",
    "whisper-large-v3-q5_0",
    "whisper-large-v3-turbo-f16",
    "whisper-large-v2",
    "whisper-medium-q8",
    "whisper-medium-en",
    "whisper-small-en",
    "whisper-medium",
    "whisper-small",
    "whisper-base",
    "whisper-tiny",
];

const MODEL_BLOCKS: [ModelBlock; 4] = [
    ModelBlock {
        tab: ModelManagerTab::SenseVoice,
        title: "SenseVoice 极速语音识别",
        subtitle: "推荐引擎：单次前向出字，自带标点与数字规整",
        categories: &[ModelCategory {
            title: "识别模型与词表",
            item_ids: &["sensevoice-model", "sensevoice-tokens", "sensevoice-vad"],
        }],
    },
    ModelBlock {
        tab: ModelManagerTab::Whisper,
        title: "Whisper 模型库",
        subtitle: "全系可选，任下一个即可用；换模型在「性能设置」或右侧抽屉里点选",
        categories: &[
            ModelCategory {
                title: "识别模型 · 快速与均衡（日常首选）",
                item_ids: WHISPER_FAST_IDS,
            },
            ModelCategory {
                title: "识别模型 · 高精度（更准、更慢、更大）",
                item_ids: WHISPER_ACCURATE_IDS,
            },
            ModelCategory {
                title: "识别模型 · 英语专用（纯英文素材更准）",
                item_ids: WHISPER_ENGLISH_IDS,
            },
            ModelCategory {
                title: "辅助模型（改善断句与长音频速度）",
                item_ids: &["silero-vad", "punc-model"],
            },
            ModelCategory {
                title: "Whisper 推理程序",
                item_ids: &["whisper-cli", "whisper-cublas"],
            },
        ],
    },
    ModelBlock {
        tab: ModelManagerTab::LocalAsr,
        title: "本地识别引擎",
        subtitle: "同一套 sherpa-onnx 运行时承载的其它模型族；体积小、语种多，按需下载",
        categories: &[ModelCategory {
            title: "多语种（东亚 / 东南亚，约 40 种语言）",
            item_ids: &["asr-dolphin"],
        }],
    },
    ModelBlock {
        tab: ModelManagerTab::Common,
        title: "公用组件",
        subtitle: "抽音解码、伴音播放，以及离线大模型（翻译 / 润色）",
        categories: &[
            ModelCategory {
                title: "音视频解码与伴音播放（FFmpeg / FFplay）",
                item_ids: &["ffmpeg", "ffplay"],
            },
            ModelCategory {
                // 推理程序 + **全部离线大模型档位**都列出来，供用户按机器性能挑一个。
                // 离线翻译/润色对模型规模很敏感：0.5B 只够「能用」，1.5B/3B 的译文
                // 通顺度明显更好。默认只下 0.5B，其余是可选升级项。
                title: "本地大模型翻译与润色引擎",
                item_ids: &["llama-cpp", "qwen-llm", "qwen-llm-1_5b", "qwen-llm-3b"],
            },
        ],
    },
];

/// 某个组件在指定专区里**是否要展示在列表中**。
///
/// # 为什么与「是否缺失」分开
///
/// 旧实现把两件事合成一个函数：它返回 false 的条目**既不计入缺项、也不出现在列表里**。
/// 而它对 Whisper 模型档位的判据是「只认真当前选中的那一档」，于是「模型库」里永远
/// 只列得出一个识别模型——用户想下 Turbo Q5，得先把档位切过去才看得见它。
///
/// 现在拆开：本函数只回答「这个专区该不该显示它」（模型库 = 全部列出，供用户自由挑选），
/// 缺项计数走 [`is_item_required_now`]，只看真正必需的那几项。
pub(crate) fn is_item_needed_for_tab(
    item_id: &str,
    tab: ModelManagerTab,
    state: &crate::app::AppState,
) -> bool {
    let _is_gpu = state.config.gpu.is_gpu_tier();
    match tab {
        ModelManagerTab::SenseVoice => {
            // SenseVoice 专区：核心识别模型、分词表、VAD 专属项，以及音视频解码抽音必需的 FFmpeg 底座
            matches!(
                item_id,
                "ffmpeg" | "sensevoice-model" | "sensevoice-tokens" | "sensevoice-vad"
            )
        }
        ModelManagerTab::Whisper => {
            // Whisper 专区：**全系模型都列出来供用户挑**（数量、体积、就位状态一目了然），
            // 加上 VAD、标点与推理程序。
            // 直接查常量表，而不是在这里再抄一份 id 列表：
            // 加一个模型只要改 `WHISPER_CHOICE_MODEL_IDS`，不会再出现
            // 「列表里有、下载时被过滤掉」这种两处漂移。
            matches!(
                item_id,
                "ffmpeg" | "whisper-cli" | "whisper-cublas" | "silero-vad" | "punc-model"
            ) || WHISPER_CHOICE_MODEL_IDS.contains(&item_id)
        }
        ModelManagerTab::LocalAsr => {
            // 本地识别引擎（非 Whisper、非 SenseVoice）：模型 + 词表 + FFmpeg 抽音。
            matches!(item_id, "ffmpeg" | "asr-dolphin")
        }
        ModelManagerTab::Common => {
            // 公用组件：FFmpeg 媒体流提取、FFplay 伴音播放、llama.cpp 推理程序，
            // 以及全部离线大模型档位（见下方 ModelCategory 的说明）。
            matches!(
                item_id,
                "ffmpeg" | "ffplay" | "llama-cpp" | "qwen-llm" | "qwen-llm-1_5b" | "qwen-llm-3b"
            )
        }
    }
}

/// 该组件在**当前配置下是否真的缺**（用于顶部提示与「一键补齐」）。
///
/// # 为什么不能复用 [`is_item_needed_for_tab`]
///
/// 那个函数现在回答的是「该不该显示」。若拿它当缺失判据，模型库里 6 个没下载的档位
/// 会被算成「缺 6 项」——而用户只需要其中一个。这里只认**跑起来真正必需**的东西：
/// 识别引擎本体（且仅当前选中的那一个）、它的运行程序、以及抽音用的 ffmpeg。
///
/// 只有 `required: true` 的条目才可能计入（当前是 whisper-small 与 ffmpeg/ffplay），
/// 外加「当前选中档位对应的那个模型」——用户主动选了某一档却没下，那才算缺。
pub fn is_item_required_now(item_id: &str, state: &crate::app::AppState) -> bool {
    let is_sv = state.whisper_model_tier == crate::app::WhisperModelTier::SenseVoice;

    match item_id {
        // 抽音解码：任何模式都需要
        "ffmpeg" => true,

        // 基础推理程序：选 Whisper 且本地一个 CLI 都没有时才算缺
        "whisper-cli" => {
            let has_cli =
                state.model_is_present("whisper-cli") || state.model_is_present("whisper-cublas");
            !is_sv && !has_cli
        }

        // SenseVoice 套件：选它时三个文件齐了才跑得起来
        "sensevoice-model" | "sensevoice-tokens" | "sensevoice-vad" => is_sv,

        // Whisper 各档模型：**只认当前选中的那一档**。没选中的档位是「可选商品」，
        // 缺与不缺都不该报警——用户下不下由他自己决定。
        "whisper-tiny" | "whisper-base" | "whisper-small" | "whisper-medium"
        | "whisper-turbo-q5" | "whisper-turbo-q8" | "whisper-large-v3" => {
            !is_sv
                && state
                    .whisper_model_tier
                    .download_item_id()
                    .map(|current| current == item_id)
                    .unwrap_or(false)
        }

        // 其余（VAD / 标点 / 大模型 / 推理程序扩展包 / 播放器）都不是阻断项：
        // 它们在用到时才生效，缺失只影响对应功能，不该让整个页面标红。
        _ => false,
    }
}

/// 「模型与组件」卡的磁盘快照：条目体积 / 是否在磁盘。
///
/// 面板此前每帧对每个条目做一次磁盘探测（`presence.is_present` + `disk_size`，
/// 10 个条目 = 每帧 10+ 次 stat；声明了 `sha256` 的条目还要整文件重算哈希），
/// 而结果只用来显示一行「已就位 N 项 · 占 X」。这里照 `views/library.rs` 的
/// `ensure_library_sizes` 范式收口：后台探一次写进 GPUI 全局，渲染只读快照。
///
/// 存在 GPUI 全局（`App::update_default_global`）而不是 `MainWindow` 字段里：
/// 字段定义在并发改动中的 `ui/mod.rs`，本模块无权改；全局随 App 生命周期存活，
/// 等价于视图级状态。
#[derive(Default)]
struct ModelDiskSnapshot {
    /// 快照基于哪个键算出（`None` = 还没算过）
    key: Option<ModelDiskKey>,
    /// 条目 id → 磁盘占用字节（`None` = 未就位 / 探测失败）
    sizes: HashMap<String, Option<u64>>,
    /// 后台探测进行中：避免每帧重复派发
    inflight: bool,
}

impl Global for ModelDiskSnapshot {}

/// 快照失效键：键变了才重扫（判定抽成纯函数 [`model_disk_snapshot_stale`]）。
///
/// - `entries`：要探测的条目 (id, 生效路径)。条目集合或生效路径变化（改了配置）
///   必须重扫；
/// - `downloading`：下载在跑时体积一直在变，下载完成 true→false 就是「立刻刷新」
///   的信号——下载完成回调在别的文件里（本模块无权改），靠键自动失效即可；
/// - `presence`：各条目就位快照（按 `ITEMS` 稳定顺序）。删除 / 补齐后
///   `AppState::refresh_model_presence` 会改它，保证「删掉模型」也自动刷新统计。
#[derive(Clone, PartialEq, Eq)]
struct ModelDiskKey {
    entries: Vec<(String, PathBuf)>,
    downloading: bool,
    presence: Vec<bool>,
}

/// 快照是否需要重扫（纯函数，便于单测）：键变了 → 重扫；键不变 → 复用。
fn model_disk_snapshot_stale(cached: Option<&ModelDiskKey>, current: &ModelDiskKey) -> bool {
    cached != Some(current)
}

/// 后台探测所有条目的磁盘占用，返回 `id → 体积`（未就位 / 探测失败为 `None`）。
///
/// 与旧渲染路径逐字同源：先 `is_present` 判定就位，再对就位项 `disk_size`，
/// 区别只是整段搬到了后台线程（`fs::metadata` 在机械盘 / 网络盘上可能卡住）。
fn scan_model_disk_usage(cfg: &crate::utils::AppConfig) -> HashMap<String, Option<u64>> {
    use crate::utils::model_download::{item_footprint, PresenceContextRef};
    let presence = PresenceContextRef::new(cfg);
    ITEMS
        .iter()
        .map(|item| {
            let size = if presence.is_present(item) {
                // 用 `item_footprint` 而不是单文件的 `disk_size(dest)`：解压型条目
                // （whisper / llama / sherpa）的 dest 只是入口文件，真实占用在同目录几十个文件里。
                item_footprint(item, cfg)
            } else {
                None
            };
            (item.id.to_string(), size)
        })
        .collect()
}

impl MainWindow {
    /// 当前快照失效键：条目集合 + 生效路径 + 下载中标志 + 就位快照。
    fn model_disk_key(&self) -> ModelDiskKey {
        let cfg = &self.state.config;
        ModelDiskKey {
            entries: ITEMS
                .iter()
                .map(|item| {
                    (
                        item.id.to_string(),
                        crate::utils::model_download::item_effective_path(item, cfg),
                    )
                })
                .collect(),
            downloading: self.state.is_downloading,
            presence: ITEMS
                .iter()
                .map(|item| self.state.model_is_present(item.id))
                .collect(),
        }
    }

    /// 读「已就位条目体积 / 条数」快照；失效时在后台重扫一次。
    ///
    /// 渲染路径只读内存：键不变就直接复用，键变了（下载完成 / 删除 / 改路径）
    /// 才派发一次后台探测，完成后 `cx.notify()` 自动刷新。
    fn model_disk_usage(&self, cx: &mut gpui::Context<Self>) -> (u64, usize) {
        let sizes = self.model_disk_sizes(cx);
        sizes
            .values()
            .copied()
            .flatten()
            .fold((0u64, 0usize), |(bytes, count), size| {
                (bytes + size, count + 1)
            })
    }

    /// 单项的**磁盘实际占用**（`None` = 未就位 / 探测失败）。
    ///
    /// # 为什么要显示实际值而不是清单里的声明值
    ///
    /// 清单里的 `size` 是「下载预计多大」，用于未下载时的预期；而已经躺在盘上的
    /// 文件未必就是清单里那个（用户可能自行量化、上游也可能改了产物），此时显示
    /// 声明值就是在撒谎——用户删掉它腾出的空间与显示的数字对不上。
    fn model_disk_sizes(
        &self,
        cx: &mut gpui::Context<Self>,
    ) -> std::collections::HashMap<String, Option<u64>> {
        let key = self.model_disk_key();
        let stale = cx.update_default_global::<ModelDiskSnapshot, _>(|state, _| {
            model_disk_snapshot_stale(state.key.as_ref(), &key) && !state.inflight
        });
        if stale {
            let cfg = self.state.config.clone();
            let key_for_store = key;
            cx.update_default_global::<ModelDiskSnapshot, _>(|state, _| state.inflight = true);
            cx.spawn(async move |this, cx| {
                let sizes = cx
                    .background_executor()
                    .spawn(async move { scan_model_disk_usage(&cfg) })
                    .await;
                let _ = this.update(cx, |_this, cx| {
                    cx.update_default_global::<ModelDiskSnapshot, _>(|state, _| {
                        state.sizes = sizes;
                        state.key = Some(key_for_store);
                        state.inflight = false;
                    });
                    cx.notify();
                });
            })
            .detach();
        }
        cx.update_default_global::<ModelDiskSnapshot, _>(|state, _| state.sizes.clone())
    }
    /// 模型管理卡片。`compact` 为真时只显示缺失项与一个「一键补齐」按钮
    /// （用于侧栏），否则显示完整清单（用于独立页面）。
    pub(crate) fn render_model_manager(
        &mut self,
        _compact: bool,
        cx: &mut gpui::Context<Self>,
    ) -> AnyElement {
        let is_downloading = self.state.is_downloading;
        let status = self.state.download_status_msg.clone();
        // 逐条目的字节进度见 `download_item_progress`（在行内渲染），
        // 这里不再取全局 done/total —— 并发时它们只代表最后一条。
        let _is_gpu = self.state.config.gpu.is_gpu_tier();

        // ── 顶部：只统计**当前配置下真正必需**的缺失项 ──
        //
        // 判据是 [`is_item_required_now`] 而不是 [`is_item_needed_for_tab`]：后者管
        // 「列表里显不显示」，模型库现在全系列出，拿它计数会把「还没决定下哪个」的
        // 可选档位算成缺项（用户看到红字「缺 6 项」，其实一个都不缺）。
        let active_missing_items: Vec<&crate::utils::DownloadItem> = ITEMS
            .iter()
            .filter(|i| !self.state.model_is_present(i.id))
            .filter(|i| is_item_required_now(i.id, &self.state))
            .collect();
        let missing = active_missing_items.len();

        // 缺项不再是「错误」：多数用户只会装一个模型，其余档位留着不装是常态。
        // 底座工具缺失才报红，模型未下提示按需下载即可。
        let missing_badge = if missing == 0 {
            primitives::badge_accent("所需组件已就位")
        } else {
            let has_tool_missing = active_missing_items
                .iter()
                .any(|i| i.id == "ffmpeg" || i.id == "whisper-cli");
            if has_tool_missing {
                primitives::badge_danger("需下载基础运行组件")
            } else {
                primitives::badge("按需下载模型即可")
            }
        };

        // 占用统计：读后台快照，渲染路径不碰磁盘（见 [`ModelDiskSnapshot`]）
        let (used_bytes, used_count) = self.model_disk_usage(cx);

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
                    .child(primitives::section_title("模型与组件"))
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
            );

        // ── 顶部动作条：只剩「取消下载」 ──
        //
        // 「下载必需组件」按钮此前是全局一个，下什么由「当前专区」决定。现在三块
        // 同页铺开，每个块头各带一个按钮、只下该块真正必需的条目——比一个全局按钮
        // 更明确：用户看着哪一块，点的就是哪一块。
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
            } else {
                None
            });

        // 进度条已改到**每个条目自己那一行**（见 `render_model_row`）：
        // 并发下载时顶部的单条进度只能显示其中一个任务，反而误导。
        // 这里只保留一句总状态文案（`download_status_msg`，在卡片底部渲染）。
        // 各条目的磁盘实际占用（渲染路径零 I/O，读后台快照）
        let disk_sizes = self.model_disk_sizes(cx);

        // ── 三个分块：同页铺开，各自可折叠 ──
        //
        // 块与块之间是**独立**的：展开状态互不影响，块头的「下载必需组件」也只下
        // 本块真正必需的条目。这样「一眼看全三块」与「逐块动手」可以同时做到。
        let mut list = div().w_full().flex().flex_col().gap(px(Theme::SPACE_3));
        for block in MODEL_BLOCKS.iter() {
            let tab = block.tab;
            let open = self.model_blocks_open[tab as usize];

            let all_ids: Vec<&'static str> = block
                .categories
                .iter()
                .flat_map(|c| c.item_ids.iter().copied())
                .collect();
            let present = all_ids
                .iter()
                .filter(|id| self.state.model_is_present(id))
                .count();

            // 本块内「当前配置真正必需且缺失」的条目 —— 决定块头那个按钮下什么。
            // 判据组合：先按块过滤显示范围，再按「真的必需」收窄，避免把整块可选
            // 档位（Whisper 全系约 5GB）都排进下载队列。
            let required_missing: Vec<&'static crate::utils::DownloadItem> = ITEMS
                .iter()
                .filter(|i| !self.state.model_is_present(i.id))
                .filter(|i| is_item_needed_for_tab(i.id, tab, &self.state))
                .filter(|i| is_item_required_now(i.id, &self.state))
                .collect();

            // 块头：**浅蓝底横条 + 蓝标题**。
            //
            // 为什么必须换色：块标题、分类标题、模型名此前全是同一个 `text_primary`
            // （#f3f4f6），而字号只差 1px（13 / 12 / 12）——一屏几十行看下来根本分不清
            // 哪一行是标题、哪一行是模型。现在把规则定死为「**蓝色 = 标题，白色 = 模型**」，
            // 扫一眼就能认出层级，不必逐字去读。
            let head = div()
                .w_full()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(Theme::SPACE_3))
                .px(px(Theme::CARD_PAD))
                .py(px(Theme::SPACE_2))
                .bg(Theme::tint_blue_soft())
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .flex()
                        .flex_col()
                        .gap(px(Theme::SPACE_0_5))
                        .child(
                            div()
                                .text_size(px(Theme::TEXT_BODY_LG))
                                .font_weight(FontWeight::BOLD)
                                .text_color(Theme::accent_blue())
                                .child(block.title),
                        )
                        .child(
                            div()
                                .text_size(px(Theme::TEXT_CAPTION))
                                .text_color(Theme::text_secondary())
                                .child(block.subtitle),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .flex_shrink_0()
                        .gap(px(Theme::SPACE_2))
                        .child(
                            div()
                                .text_size(px(Theme::TEXT_CAPTION))
                                .text_color(Theme::text_muted())
                                .child(format!("已就位 {present} / {}", all_ids.len())),
                        )
                        .children((!required_missing.is_empty() && !is_downloading).then(|| {
                            let n = required_missing.len();
                            let btn_label = match tab {
                                ModelManagerTab::SenseVoice => {
                                    if n == 1 {
                                        "下载 SenseVoice 套件".to_string()
                                    } else {
                                        format!("下载 SenseVoice（{n} 项）")
                                    }
                                }
                                ModelManagerTab::Whisper => "下载选用模型".to_string(),
                                ModelManagerTab::LocalAsr => "下载本地识别模型".to_string(),
                                ModelManagerTab::Common => {
                                    if n == 1 {
                                        "下载基础组件".to_string()
                                    } else {
                                        format!("下载基础组件（{n} 项）")
                                    }
                                }
                            };
                            primitives::btn_clickable(
                                btn_label,
                                primitives::BtnSize::Sm,
                                primitives::BtnVariant::Primary,
                            )
                            .id(("model-block-dl", tab as usize))
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.download_all_missing(tab, cx);
                                },
                            ))
                        }))
                        .child(
                            primitives::mini_btn(if open { "收起" } else { "展开" }, true)
                                .id(("model-block-toggle", tab as usize))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    let slot = &mut this.model_blocks_open[tab as usize];
                                    *slot = !*slot;
                                    cx.notify();
                                })),
                        ),
                );

            // 内边距移交给 head 与 body 各自负责：块头要铺满整宽（才能出横条效果），
            // 不能再让外层统一留白。`overflow_hidden` 负责把横条的方角裁进卡片圆角里。
            let mut section = div()
                .w_full()
                .flex()
                .flex_col()
                .rounded(px(Theme::CARD_RADIUS))
                .bg(Theme::bg_card())
                .border_1()
                .border_color(Theme::border())
                .overflow_hidden()
                .child(head);

            if open {
                let mut body = div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap(px(Theme::SPACE_3))
                    .p(px(Theme::CARD_PAD));
                for cat in block.categories {
                    let cat_items: Vec<_> = cat
                        .item_ids
                        .iter()
                        .filter_map(|&id| ITEMS.iter().find(|i| i.id == id))
                        .filter(|i| is_item_needed_for_tab(i.id, tab, &self.state))
                        .collect();

                    if cat_items.is_empty() {
                        continue;
                    }

                    // 分类标题不按「缺几项」标红：模型库是一份可选清单，没下完是常态。
                    // 改为中性统计「已就位 / 共几项」，让用户知道进度而不是被指责。
                    let cat_present = cat_items
                        .iter()
                        .filter(|i| self.state.model_is_present(i.id))
                        .count();
                    let cat_total = cat_items.len();

                    let is_choice_cat = cat.title.contains("任选其一");
                    let choice_lacking = is_choice_cat && cat_present == 0;

                    let mut col = div().w_full().flex().flex_col().gap(px(Theme::SPACE_1_5));
                    col = col.child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(Theme::SPACE_1_5))
                                    // 小圆点与标题：采用温和的中性蓝/薄荷绿引导，不搞大面积报错红色
                                    .child(primitives::stat_dot_sm(Theme::accent_blue()))
                                    .child(
                                        div()
                                            .text_size(px(Theme::TEXT_BODY))
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(Theme::accent_blue())
                                            .child(cat.title),
                                    ),
                            )
                            .child(
                                div()
                                    .text_size(px(Theme::TEXT_CAPTION))
                                    .text_color(Theme::text_muted())
                                    .child(if choice_lacking {
                                        format!("待选其一 · 0 / {cat_total}")
                                    } else {
                                        format!("已就位 {cat_present} / {cat_total}")
                                    }),
                            ),
                    );
                    for item in cat_items {
                        let actual = disk_sizes.get(item.id).copied().flatten();
                        col = col.child(self.render_model_row(item, is_downloading, actual, cx));
                    }
                    body = body.child(col);
                }
                section = section.child(body);
            }
            list = list.child(section);
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

        // 版本更新横幅（发现新版本或已是最新版）
        if let Some((newer, text, _url)) = self.state.update_check_result.clone() {
            let is_downloading = self.state.update_download_busy;
            let downloaded = self.state.update_downloaded_path.is_some();
            let has_target = self.state.update_download_target.is_some();
            let progress = self.state.update_download_progress;

            if newer {
                card = card.child(
                    div()
                        .w_full()
                        .p(px(Theme::SPACE_2_5))
                        .rounded_md()
                        .bg(Theme::bg_inset())
                        .border_1()
                        .border_color(Theme::accent_mint())
                        .flex()
                        .flex_col()
                        .gap(px(Theme::SPACE_2))
                        .child(
                            div()
                                .w_full()
                                .flex()
                                .items_center()
                                .justify_between()
                                .gap(px(Theme::SPACE_2))
                                .child(
                                    div()
                                        .text_size(px(Theme::TEXT_SMALL))
                                        .font_weight(FontWeight::BOLD)
                                        .text_color(Theme::accent_mint())
                                        .child(
                                            if let Some(status) = &self.state.update_download_status
                                            {
                                                status.clone()
                                            } else {
                                                text
                                            },
                                        ),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(px(Theme::SPACE_2))
                                        .child(
                                            primitives::chip_clickable("查看更新详情", true, false)
                                                .id("update-view-modal-btn")
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.update_modal_open = true;
                                                    cx.notify();
                                                })),
                                        )
                                        .children(if downloaded {
                                            Some(
                                                primitives::chip_clickable(
                                                    "立即安装升级",
                                                    true,
                                                    false,
                                                )
                                                .id("update-install-btn")
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.install_downloaded_update(cx);
                                                })),
                                            )
                                        } else if is_downloading {
                                            Some(
                                                primitives::chip_clickable(
                                                    "正在下载…",
                                                    false,
                                                    false,
                                                )
                                                .id("update-downloading-btn"),
                                            )
                                        } else if has_target {
                                            Some(
                                                primitives::chip_clickable(
                                                    "直接下载更新",
                                                    true,
                                                    false,
                                                )
                                                .id("update-direct-download-btn")
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.download_app_update(cx);
                                                })),
                                            )
                                        } else {
                                            None
                                        }),
                                ),
                        )
                        .children(if is_downloading {
                            Some(
                                div()
                                    .w_full()
                                    .h(px(4.0))
                                    .rounded_full()
                                    .bg(Theme::bg_raised())
                                    .child(
                                        div()
                                            .h_full()
                                            .rounded_full()
                                            .bg(Theme::accent_mint())
                                            .w(relative(progress.clamp(0.01, 1.0))),
                                    ),
                            )
                        } else {
                            None
                        }),
                );
            } else {
                card = card.child(
                    div()
                        .w_full()
                        .px(px(Theme::SPACE_2_5))
                        .py(px(Theme::SPACE_1_5))
                        .rounded_md()
                        .bg(Theme::bg_inset())
                        .border_1()
                        .border_color(Theme::border_light())
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .text_size(px(Theme::TEXT_SMALL))
                                .text_color(Theme::text_secondary())
                                .child(format!(
                                    "✓ 当前已是最新版本 (v{})，无需更新",
                                    crate::utils::update_check::current_version()
                                )),
                        )
                        .child(
                            div()
                                .text_size(px(Theme::TEXT_CAPTION))
                                .text_color(Theme::text_muted())
                                .child("已与 GitHub 官方源同步"),
                        ),
                );
            }
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

    /// 「跑这个模型需要什么样的机器」——悬浮卡片的内容来源。
    ///
    /// 为什么单独一张表而不是塞进 DownloadItem：那个结构是下载/校验用的数据结构，
    /// 硬件建议属于**展示**，改文案不该动下载逻辑，也不该让 30+ 个字面量再长一截。
    pub(crate) fn hardware_hint(item_id: &str) -> &'static str {
        match item_id {
            "whisper-tiny" => "内存 2 GB · 纯 CPU 可跑（最快）· 磁盘 75 MB",
            "whisper-base" => "内存 2 GB · 纯 CPU 可跑 · 磁盘 141 MB",
            "whisper-small" => "内存 4 GB · 纯 CPU 可跑 · 磁盘 173 MB",
            "whisper-small-en" => "内存 4 GB · 纯 CPU 可跑 · 磁盘 465 MB（仅英文）",
            "whisper-medium" => "内存 8 GB · 纯 CPU 可跑但偏慢 · 磁盘 1.5 GB",
            "whisper-medium-q8" => "内存 6 GB · 纯 CPU 可跑 · 磁盘 785 MB",
            "whisper-medium-en" => "内存 8 GB · 纯 CPU 偏慢 · 磁盘 1.5 GB（仅英文）",
            "whisper-turbo-q5" => "内存 6 GB · 建议 4 核以上 · 磁盘 547 MB",
            "whisper-turbo-q8" => "内存 8 GB · 建议 4 核以上 · 磁盘 834 MB",
            "whisper-large-v3-turbo-f16" => "内存 12 GB · 建议独显 · 磁盘 1.6 GB",
            "whisper-large-v3" => "内存 16 GB · 建议 6 GB 以上显存 · 磁盘 3.0 GB",
            "whisper-large-v3-q5_0" => "内存 10 GB · 建议独显 · 磁盘 1.1 GB",
            "whisper-large-v2" => "内存 16 GB · 建议 6 GB 以上显存 · 磁盘 3.0 GB",
            "whisper-cublas" => "需要 NVIDIA 显卡 + CUDA 运行库 · 磁盘 436 MB",
            "whisper-cli" => "纯 CPU 即可（CPU 版推理程序）· 磁盘 4 MB",
            "sensevoice-model" => "内存 4 GB · 纯 CPU 即可，速度最快（非自回归）· 磁盘 228 MB",
            "punc-model" => "内存 2 GB · 纯 CPU · 磁盘 72 MB",
            "silero-vad" => "内存 <1 GB · 纯 CPU · 磁盘 1 MB",
            "asr-dolphin" => "内存 4 GB · 纯 CPU 可跑 · 解压后约 80 MB",
            _ => "纯 CPU 即可 · 具体占用见左侧大小",
        }
    }

    /// 悬浮卡片：悬停某行时浮在行右侧，显示「需要什么配置」。
    ///
    /// 用绝对定位而不是插入一行：卡片一插进去就会把整张列表顶开，鼠标一动布局就跳，
    /// 反而看不清。这里只浮不改布局。
    fn render_model_hover_card(item: &'static crate::utils::DownloadItem) -> AnyElement {
        div()
            .absolute()
            .top(px(0.0))
            .right(px(0.0))
            .w(px(300.0))
            .p(px(Theme::SPACE_2_5))
            .rounded(px(Theme::RADIUS_SM))
            .bg(Theme::bg_raised())
            .border_1()
            .border_color(Theme::border())
            .shadow_lg()
            .flex()
            .flex_col()
            .gap(px(Theme::SPACE_1))
            .child(
                div()
                    .text_size(px(Theme::TEXT_CAPTION))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(Theme::text_primary())
                    .child("配置要求"),
            )
            .child(
                div()
                    .text_size(px(Theme::TEXT_CAPTION))
                    .text_color(Theme::text_muted())
                    .child(Self::hardware_hint(item.id)),
            )
            .into_any_element()
    }

    /// 单个条目行：名称 + 用途说明 + 体积 + 状态徽标 + 操作区。
    fn render_model_row(
        &mut self,
        item: &'static crate::utils::DownloadItem,
        is_downloading: bool,
        actual_size: Option<u64>,
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
        // 这一条自己的下载进度（没有 = 没在下）。
        let row_progress = self.state.download_item_progress.get(item.id).copied();
        // 悬停回调拿到的只有 &mut App，所以要提前把实体句柄降级保存一份。
        let hover_entity = cx.entity().downgrade();
        // 当前配置下是否**真的需要**它（只认真必需的，见 [`is_item_required_now`]）。
        // 未就位不再一律染红：模型列表是一份可选清单，没下过的档位是「还没装」
        // 而不是「出错了」。
        let is_required_now = is_item_required_now(item.id, &self.state);
        let is_base_tool = item.id == "ffmpeg" || item.id == "whisper-cli";

        // Whisper 任选其一模型组判定：
        let is_whisper_choice = WHISPER_CHOICE_MODEL_IDS.contains(&item.id);
        let any_whisper_choice_present = WHISPER_CHOICE_MODEL_IDS
            .iter()
            .any(|&id| self.state.model_is_present(id));

        // 如果用户连一个 Whisper 识别模型都还没下 (0 / 7 就位)：
        // 这一组全都是候选，全部标红警示「任选其一待下载」；
        // 一旦用户下载了其中任意一个（any_whisper_choice_present == true），
        // 这一组的刚性要求即告满足，其它未下的模型恢复中性可选，全部「不红了」！
        let is_choice_lacking = is_whisper_choice && !any_whisper_choice_present;

        // 仅底座工具缺失时才标红；模型属于可选候选档位，不论 CPU/GPU 均可按需下载，不应整行大红警示
        let is_row_danger = !present && !is_downloading && (is_base_tool && is_required_now);

        // 状态徽标：已就位 / 必需底座缺失 / 当前选用 / 任选其一 / 可选
        let status_badge = if is_current && is_downloading {
            primitives::badge_accent("下载中")
        } else if custom {
            primitives::badge_accent("已就位（自编译）")
        } else if present {
            primitives::badge_accent("已就位")
        } else if is_base_tool && is_required_now {
            // 真正的运行底座缺失：标红警示
            primitives::badge_danger("必需组件 · 待下载")
        } else if is_required_now {
            // 当前选用的推荐/默认档：主色突出引导
            primitives::badge_accent("当前选用 · 待下载")
        } else if is_choice_lacking {
            // 一个模型都没下时的备选模型：中性徽标，不搞大面积报错红色
            primitives::badge("任选其一 · 待下载")
        } else if item.required {
            primitives::badge("未下载 · 下载后可用")
        } else {
            // 可选档位：中性徽标，不催不吓（用户想装哪个自己决定）
            primitives::badge("未下载 · 可选")
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
                        } else {
                            "下载"
                        },
                        primitives::BtnSize::Sm,
                        // 当前配置必需的、或连一个模型都未就位时的备选模型用主按钮，
                        // 已就位至少一个后的可选档位用次按钮。
                        if is_row_danger || is_required_now {
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
            // 只有「本地识别引擎」族的条目才有「设为当前」这件事：
            // 它们是同一套运行时的不同模型，切换靠改 config 的两个路径。
            if crate::engines::sensevoice::model_family_for_item(item.id).is_some() {
                actions = actions.child(
                    primitives::mini_btn("设为当前", !is_downloading)
                        .id(("model-use-asr", row_idx))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.use_local_asr_model(row_id, cx);
                        })),
                );
            }
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

        if is_row_danger {
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
                                    .text_color(if is_row_danger {
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
                                    // 已就位显示**磁盘实际占用**（用户据此判断能腾多少空间），
                                    // 未就位才显示清单里的预计体积。两种口径混用会让
                                    // 「显示 172.9 MB、删掉却腾出 465 MB」这种事发生。
                                    .child(match actual_size {
                                        Some(bytes) => human_size(bytes),
                                        None => format!("约 {}", human_size(item.size)),
                                    }),
                            )
                            // 下载中：进度直接长在**这一条自己**的下面。
                            // 同时下几条时各看各的，不用去顶部找「现在是谁在跑」。
                            .children(row_progress.map(|(done, total)| {
                                let ratio = if total > 0 {
                                    (done as f32 / total as f32).clamp(0.0, 1.0)
                                } else {
                                    0.0
                                };
                                div()
                                    .w_full()
                                    .mt(px(Theme::SPACE_1))
                                    .flex()
                                    .flex_col()
                                    .gap(px(Theme::SPACE_0_5))
                                    .child(primitives::progress_bar(ratio))
                                    .child(
                                        div()
                                            .text_size(px(Theme::TEXT_CAPTION))
                                            .text_color(Theme::text_muted())
                                            .child(if total > 0 {
                                                format!(
                                                    "{} / {}",
                                                    human_size(done),
                                                    human_size(total)
                                                )
                                            } else {
                                                // 服务端没给 Content-Length：只报已下载量，不假装知道总大小。
                                                human_size(done)
                                            }),
                                    )
                            }))
                            .child(status_badge),
                    ),
            )
            .child(right_actions)
            // 交互元素必须先有稳定 id，GPUI 才把 on_hover 挂得上（Div 只有在
            // 实现 StatefulInteractiveElement 之后才提供 on_hover）。
            .id(("model-row", row_idx))
            // 悬停：只写「谁被悬停」，卡片浮在行右侧，不改变任何一行的高度。
            .on_hover(move |hovered: &bool, _window, app| {
                let on = *hovered;
                let _ = hover_entity.update(app, |this, cx| {
                    let next = if on { Some(row_id) } else { None };
                    if this.state.hovered_model_id != next {
                        this.state.hovered_model_id = next;
                        cx.notify();
                    }
                });
            })
            .when(self.state.hovered_model_id == Some(item.id), |d| {
                d.child(Self::render_model_hover_card(item))
            })
            .into_any_element()
    }
}
#[cfg(test)]
mod tests {
    use super::{model_disk_snapshot_stale, ModelDiskKey};
    use std::path::PathBuf;

    /// 分块表必须自洽：引用的条目 id 都真实存在，且**同一 id 只出现在一个块里**。
    ///
    /// 后者是「一页三块」特有的风险。早先按专区切换时，同一个 id 登记在两个专区
    /// 是无害的（一次只渲染一个）；现在三块同时渲染，重复登记会让同一个模型在页面上
    /// 出现两次，块头「已就位 N / M」的分母也会被算重。
    #[test]
    fn block_table_references_real_items_without_duplicates() {
        use super::MODEL_BLOCKS;
        use crate::ui::ModelManagerTab;
        use crate::utils::ITEMS;
        use std::collections::HashMap;

        let mut seen: HashMap<&'static str, ModelManagerTab> = HashMap::new();
        for block in MODEL_BLOCKS.iter() {
            for cat in block.categories {
                assert!(!cat.item_ids.is_empty(), "分类「{}」没有条目", cat.title);
                for &id in cat.item_ids {
                    assert!(
                        ITEMS.iter().any(|i| i.id == id),
                        "分块「{}」引用了不存在的条目 id：{id}",
                        block.title
                    );
                    if let Some(prev) = seen.insert(id, block.tab) {
                        panic!(
                            "条目 {id} 被重复登记（已在 {prev:?}，又出现在 {:?}）——\
                             三块同页渲染时它会出现两次",
                            block.tab
                        );
                    }
                }
            }
        }
        // 每块都必须出现在表里：少一块就等于把那个专区藏起来了，正是本次要根治的问题。
        // 数量随之变成 4（新增「本地识别引擎」），断言跟着改——但「缺一不可」的语义不变。
        assert_eq!(MODEL_BLOCKS.len(), 4, "四个分块缺一不可");
        // 每个 tab 恰好出现一次，避免漏加或重复加。
        for tab in [
            ModelManagerTab::SenseVoice,
            ModelManagerTab::Whisper,
            ModelManagerTab::LocalAsr,
            ModelManagerTab::Common,
        ] {
            assert_eq!(
                MODEL_BLOCKS.iter().filter(|b| b.tab == tab).count(),
                1,
                "分块表里 {tab:?} 必须恰好出现一次"
            );
        }
    }

    fn key(entries: &[(&str, &str)], downloading: bool, presence: &[bool]) -> ModelDiskKey {
        ModelDiskKey {
            entries: entries
                .iter()
                .map(|&(id, path)| (id.to_string(), PathBuf::from(path)))
                .collect(),
            downloading,
            presence: presence.to_vec(),
        }
    }

    /// 键完全一致 → 复用旧快照，不重扫（这是「渲染路径不再碰磁盘」的前提）。
    #[test]
    fn model_disk_snapshot_reuses_when_key_unchanged() {
        let k = key(&[("a", "x.bin")], false, &[true]);
        assert!(!model_disk_snapshot_stale(Some(&k), &k), "键不变时不该重扫");
    }

    /// 从没算过（无缓存）→ 必须重扫一次。
    #[test]
    fn model_disk_snapshot_scans_when_never_computed() {
        let k = key(&[("a", "x.bin")], false, &[true]);
        assert!(model_disk_snapshot_stale(None, &k), "首次必须扫一次");
    }

    /// 下载完成（downloading true→false）→ 键变化 → 重扫，统计自动刷新。
    #[test]
    fn model_disk_snapshot_rescans_when_download_finishes() {
        let cached = key(&[("a", "x.bin")], true, &[false]);
        let current = key(&[("a", "x.bin")], false, &[true]);
        assert!(
            model_disk_snapshot_stale(Some(&cached), &current),
            "下载完成必须触发重扫"
        );
    }

    /// 生效路径变化（改配置）→ 键变化 → 重扫。
    #[test]
    fn model_disk_snapshot_rescans_when_path_changes() {
        let cached = key(&[("a", "x.bin")], false, &[true]);
        let current = key(&[("a", "y.bin")], false, &[true]);
        assert!(
            model_disk_snapshot_stale(Some(&cached), &current),
            "路径变了必须重扫"
        );
    }

    /// 就位快照变化（删除 / 补齐）→ 键变化 → 重扫。
    #[test]
    fn model_disk_snapshot_rescans_when_presence_changes() {
        let cached = key(&[("a", "x.bin")], false, &[true]);
        let current = key(&[("a", "x.bin")], false, &[false]);
        assert!(
            model_disk_snapshot_stale(Some(&cached), &current),
            "就位状态变了必须重扫"
        );
    }
}
