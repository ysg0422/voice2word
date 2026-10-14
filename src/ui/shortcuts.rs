//! 全局快捷键 (F-017)
//!
//! 设计取舍：**修饰键组合负责全局生效；裸键只保留「焦点不在输入框时才生效」的最小交互集**。
//!
//! 主界面的字幕搜索框、在线翻译配置框都是 GPUI 自绘输入（见 `apply_line_edit`），
//! 靠 `on_key_down` 直接插入字符。键位表是全局的，命中动作后字符就不再进入输入框，
//! 因此裸键绑定（空格 = 播放/暂停、Delete / Backspace = 删除当前句）与打字天然冲突。这里的折中是：
//! 裸键只保留 `BARE_KEYS` 这一小撮高频动作，**是否真的生效由各视图的 `.on_action(...)`
//! 自己按焦点判断**——本模块只做动作定义与键位注册，不碰 `MainWindow`。其余动作仍全部
//! 走修饰键组合，不会出现在正常打字路径上，可以全局安全生效。
//!
//! 绑定集中在 [`bind_default_keys`]（启动时注册一次），动作处理散落在各视图的
//! `.on_action(...)` 上，避免把业务逻辑堆进这个模块。
//!
//! 键位表 [`SHORTCUT_SPECS`] 同时携带速查表文案，[`SHORTCUT_HINTS`] 由同一份
//! 动作清单校验（见本文件末尾的测试）：改键位时不会再出现「界面上写着按 A、
//! 实际要按 B」的漂移。

use gpui::{actions, KeyBinding, Keystroke};
use std::collections::{BTreeMap, HashMap, HashSet};

actions!(
    voice2word,
    [
        /// 播放 / 暂停当前预览
        TogglePlayback,
        /// 跳到上一句字幕
        PrevSegment,
        /// 跳到下一句字幕
        NextSegment,
        /// 后退 1 秒
        SeekBackward,
        /// 前进 1 秒
        SeekForward,
        /// 开始（或重新开始）当前文件的转写
        StartTranscription,
        /// 关闭浮层；无浮层时终止正在进行的转写
        CancelOrClose,
        /// 按剪辑台当前选中的格式导出
        ExportSubtitle,
        /// 聚焦字幕清单搜索框
        FocusSubtitleSearch,
        /// 撤销上一次字幕编辑
        Undo,
        /// 重做上一次被撤销的字幕编辑
        Redo,
        /// 打开 / 关闭命令面板 (P1-A10)
        OpenCommandPalette,
        /// 打开 / 关闭字幕「查找替换」面板
        OpenReplacePanel,
        /// 打开 / 关闭快捷键速查面板 (F1)
        OpenShortcutsHelp,
        /// 裸空格（焦点不在输入框时）：播放 / 暂停当前预览
        TogglePlaybackBare,
        /// 裸 Delete / Backspace（焦点不在输入框时）：删除当前选中的字幕片段
        DeleteActiveSegment,
    ]
);

/// 一条快捷键的唯一定义。
///
/// `key` 是传给 [`KeyBinding::new`] 的键位描述，同时也驱动 [`binding_for`] 的
/// 分派；`hint_key` / `hint` 是速查表 [`SHORTCUT_HINTS`] 里的展示文本。
/// 展示文本与实际绑定放在同一张表里，两条绑定也可以共用同一行文案
/// （`Alt+↑↓` 就覆盖了上下两个方向键），因此不存在「速查表和实际按键对不上」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShortcutSpec {
    /// 稳定动作名（配置 `[shortcuts] overrides` 的键），如 `toggle_play`。
    ///
    /// 与 [`Self::key`] 分开：`key` 是**当前生效**的键位描述（可能被用户覆盖），
    /// `action` 是这条绑定属于哪个动作、永不改变。用户覆盖按动作名定位，改了键位
    /// 也不会失联。同一个动作可以有多条绑定（`redo` 同时占 `Ctrl+Shift+Z` 与
    /// `Ctrl+Y`），它们共用同一个 `action`。
    pub action: &'static str,
    /// 键位描述，如 `ctrl-k`
    pub key: &'static str,
    /// 速查表里的键位文本，如 `Ctrl+K`
    pub hint_key: &'static str,
    /// 速查表里的说明文本
    pub hint: &'static str,
}

/// 裸键（不带修饰键）绑定的键位描述。
///
/// 这些键位落在正常打字路径上，只有在焦点不在任何输入框时才该生效；真正的门控写在
/// 各视图的 `.on_action(...)` 里（本模块只负责注册键位）。集中成一张表是为了让测试
/// 能一眼看清「哪些裸键被占用了」，避免以后又悄悄绑一个裸字符键。
pub const BARE_KEYS: [&str; 3] = ["space", "delete", "backspace"];

/// [`SHORTCUT_SPECS`] 的条目数：定长数组与测试共用同一个常量，
/// 以后加绑定只改这一处，不会再出现「表里加了、长度常量忘了改」。
pub const SPEC_COUNT: usize = 18;

/// 默认键位表的唯一事实来源：启动时按此注册，速查表也按此校验。
pub const SHORTCUT_SPECS: [ShortcutSpec; SPEC_COUNT] = [
    ShortcutSpec {
        action: "toggle_play",
        key: "ctrl-space",
        hint_key: "Ctrl+Space",
        hint: "播放/暂停",
    },
    ShortcutSpec {
        action: "prev_segment",
        key: "alt-up",
        hint_key: "Alt+↑↓",
        hint: "上/下句",
    },
    ShortcutSpec {
        action: "next_segment",
        key: "alt-down",
        hint_key: "Alt+↑↓",
        hint: "上/下句",
    },
    ShortcutSpec {
        action: "seek_backward",
        key: "alt-left",
        hint_key: "Alt+←→",
        hint: "±1 秒",
    },
    ShortcutSpec {
        action: "seek_forward",
        key: "alt-right",
        hint_key: "Alt+←→",
        hint: "±1 秒",
    },
    ShortcutSpec {
        action: "start_transcription",
        key: "ctrl-enter",
        hint_key: "Ctrl+Enter",
        hint: "开始转写",
    },
    ShortcutSpec {
        action: "cancel_or_close",
        key: "escape",
        hint_key: "Esc",
        hint: "关闭/终止",
    },
    ShortcutSpec {
        action: "export_subtitle",
        key: "ctrl-e",
        hint_key: "Ctrl+E",
        hint: "导出",
    },
    ShortcutSpec {
        action: "focus_subtitle_search",
        key: "ctrl-f",
        hint_key: "Ctrl+F",
        hint: "搜索",
    },
    ShortcutSpec {
        action: "open_command_palette",
        key: "ctrl-k",
        hint_key: "Ctrl+K",
        hint: "命令面板",
    },
    ShortcutSpec {
        action: "undo",
        key: "ctrl-z",
        hint_key: "Ctrl+Z",
        hint: "撤销",
    },
    ShortcutSpec {
        action: "redo",
        key: "ctrl-shift-z",
        hint_key: "Ctrl+Shift+Z",
        hint: "重做",
    },
    // Windows 用户习惯用 Ctrl+Y 重做，而浏览器/编辑器过来的用户习惯 Ctrl+Shift+Z，
    // 两套都留着才不别扭。键位描述各不相同，但速查表里各占一行，用户两套都能看到。
    ShortcutSpec {
        action: "redo",
        key: "ctrl-y",
        hint_key: "Ctrl+Y",
        hint: "重做",
    },
    // Ctrl+H 是「查找替换」的通用约定（VS Code / Word / 浏览器都是它）。
    // 搜索已经占了 Ctrl+F，替换若没有独立键位，用户只能靠鼠标去点那个小按钮。
    ShortcutSpec {
        action: "open_replace_panel",
        key: "ctrl-h",
        hint_key: "Ctrl+H",
        hint: "查找替换",
    },
    // F1 是「帮助」的通用约定（浏览器、Office、各类编辑器都是它），而速查面板只做
    // 展示、不碰任何数据，可以全局安全生效；绑成 Ctrl+? 之类的组合反而没人会按。
    ShortcutSpec {
        action: "open_shortcuts_help",
        key: "f1",
        hint_key: "F1",
        hint: "快捷键速查",
    },
    // 裸键最小交互集：只在焦点不在输入框时由 `.on_action` 放行（见 `BARE_KEYS`）。
    // 空格是播放/暂停的通用约定（播放器、剪辑软件都是它），与 Ctrl+Space 并存：
    // 前者给「手在鼠标上」的场景，后者给「正在打字」的场景，两条互不覆盖。
    ShortcutSpec {
        action: "toggle_play_bare",
        key: "space",
        hint_key: "Space",
        hint: "播放/暂停（裸键）",
    },
    // Delete 删除当前选中句，也是剪辑台里最顺手的一键操作。
    ShortcutSpec {
        action: "delete_active_segment",
        key: "delete",
        hint_key: "Delete",
        hint: "删除当前句（裸键）",
    },
    // Backspace 与 Delete 同源：没有独立 Delete 键的键盘上，退格才是唯一顺手的删除键。
    // 两条裸键绑定同一个动作（`DeleteActiveSegment`），焦点在输入框时都由 `.on_action` 放行。
    ShortcutSpec {
        action: "delete_active_segment",
        key: "backspace",
        hint_key: "Backspace",
        hint: "删除当前句（裸键）",
    },
];

/// 把一条**默认**键位描述解析成具体的 [`KeyBinding`]。
///
/// 先按键位描述在 [`SHORTCUT_SPECS`] 里找到动作名，再交给 [`binding_from_action`]。
/// 这样「有哪些绑定」只由 `SHORTCUT_SPECS` 决定：新增一条绑定只需改表 + 在
/// [`binding_from_action`] 里加一个动作分支，速查表校验测试会在漏掉任一边时报错。
/// 未登记的键位描述仍然 panic（对调用方是编程错误，不该静默吞掉）。
///
/// 只在测试里用：注册走 [`bind_keys_with_overrides`]（它按动作名合成），
/// 而校验「默认键位表本身自洽」的测试需要一个「按默认键位取绑定」的入口。
#[cfg(test)]
fn binding_for(key: &str) -> KeyBinding {
    let spec = SHORTCUT_SPECS
        .iter()
        .find(|spec| spec.key == key)
        .unwrap_or_else(|| panic!("SHORTCUT_SPECS 里有未登记的键位描述: {key}"));
    binding_from_action(spec.action, key)
        .unwrap_or_else(|| panic!("SHORTCUT_SPECS 里有未登记的动作名: {}", spec.action))
}

/// 把一个**动作名** + 键位描述合成一条 [`KeyBinding`]；动作名未登记时返回 `None`。
///
/// 与 [`binding_for`] 分开，是因为用户覆盖后的键位描述不在 [`SHORTCUT_SPECS`] 里：
/// [`binding_for`] 认「默认键位描述」，这里认「动作名」，覆盖流程只能走这条路。
/// 返回 `Option` 而不是直接 panic，是为了让未知动作名能被 [`resolve_bindings`]
/// 忽略并告警，而不是把进程打崩。
fn binding_from_action(action: &str, key: &str) -> Option<KeyBinding> {
    Some(match action {
        "toggle_play" => KeyBinding::new(key, TogglePlayback, None),
        "prev_segment" => KeyBinding::new(key, PrevSegment, None),
        "next_segment" => KeyBinding::new(key, NextSegment, None),
        "seek_backward" => KeyBinding::new(key, SeekBackward, None),
        "seek_forward" => KeyBinding::new(key, SeekForward, None),
        "start_transcription" => KeyBinding::new(key, StartTranscription, None),
        "cancel_or_close" => KeyBinding::new(key, CancelOrClose, None),
        "export_subtitle" => KeyBinding::new(key, ExportSubtitle, None),
        "focus_subtitle_search" => KeyBinding::new(key, FocusSubtitleSearch, None),
        "open_command_palette" => KeyBinding::new(key, OpenCommandPalette, None),
        "undo" => KeyBinding::new(key, Undo, None),
        "redo" => KeyBinding::new(key, Redo, None),
        "open_replace_panel" => KeyBinding::new(key, OpenReplacePanel, None),
        "open_shortcuts_help" => KeyBinding::new(key, OpenShortcutsHelp, None),
        // 裸键：与上面的修饰键组合是两条独立绑定，键位描述不会互相覆盖。
        "toggle_play_bare" => KeyBinding::new(key, TogglePlaybackBare, None),
        // Backspace 与 Delete 同源：键位描述不同，动作名相同。
        "delete_active_segment" => KeyBinding::new(key, DeleteActiveSegment, None),
        _ => return None,
    })
}

/// 一条经「默认表 + 用户覆盖」合成后的绑定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedBinding {
    /// 稳定动作名（[`ShortcutSpec::action`]）
    pub action: &'static str,
    /// 生效的键位描述（规范化小写）
    pub key: String,
    /// 速查表里的键位文本：覆盖生效时按覆盖键重新排版，否则沿用内置文案
    pub hint_key: String,
    /// 速查表里的说明文本
    pub hint: &'static str,
    /// `key` 是否来自一条**真正生效**的用户覆盖
    pub is_override: bool,
    /// 该动作的覆盖因冲突被驳回时，记下被驳回的原始覆盖键（供速查表标注）
    pub ignored_override: Option<String>,
}

/// 一次键位合成里检测到的一处冲突。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    /// 被两条绑定争用的键位（规范化小写）
    pub key: String,
    /// 赢家动作：保留该键位的那个（默认绑定优先）
    pub kept_action: &'static str,
    /// 输家动作：覆盖被驳回、回退到默认键位的那个
    pub dropped_action: &'static str,
    /// 可读的冲突说明（写日志，之后也可接进界面提示）
    pub detail: String,
}

/// 合成结果：最终生效的绑定表。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedBindings {
    /// 最终绑定，顺序与 `specs` 对齐（同动作的多条绑定各自保留）
    pub bindings: Vec<ResolvedBinding>,
}

/// 合成过程中的一条候选绑定（尚未消解冲突）。
struct Candidate {
    /// 在 `specs` 里的下标，决定消解冲突时的先后
    index: usize,
    action: &'static str,
    /// 生效键位（覆盖键或默认键，规范化小写）
    key: String,
    /// 该动作的默认键位：覆盖被驳回时回退到它
    default_key: String,
    /// 默认速查表键位文本
    default_hint_key: &'static str,
    hint: &'static str,
    is_override: bool,
}

/// 规范化键位描述：去首尾空白 + 转小写，作为冲突判定的统一口径。
fn normalize_key(key: &str) -> String {
    key.trim().to_lowercase()
}

/// 键位描述是否是一个可解析的单键序列。
///
/// 用 GPUI 自己的 [`Keystroke::parse`] 判定，而不是另写一套白名单：只有它认识的
/// 键位才真正能注册成功。空串、纯空白、以及用空白分隔的多键序列（面板里每条绑定
/// 只吃一个键位）一律视为非法。
fn is_valid_key(key: &str) -> bool {
    if key.is_empty() || key.split_whitespace().count() != 1 {
        return false;
    }
    Keystroke::parse(key).is_ok()
}

/// 把键位描述排版成速查表文案：`ctrl-shift-alt-k` → `Ctrl+Shift+Alt+K`。
///
/// 只用于**用户覆盖**的键位：默认键位有手写的展示文案（`Alt+↑↓` 这类合并不可能
/// 靠机械排版还原），覆盖键没有，只能按这个规则排。
fn format_key_label(key: &str) -> String {
    let parts: Vec<String> = key
        .split('-')
        .map(|token| match token {
            "ctrl" | "control" => "Ctrl".to_string(),
            "shift" => "Shift".to_string(),
            "alt" | "option" => "Alt".to_string(),
            "super" | "cmd" | "command" | "win" | "meta" => "Win".to_string(),
            "space" => "Space".to_string(),
            "up" => "↑".to_string(),
            "down" => "↓".to_string(),
            "left" => "←".to_string(),
            "right" => "→".to_string(),
            "escape" | "esc" => "Esc".to_string(),
            "enter" | "return" => "Enter".to_string(),
            "delete" | "del" => "Delete".to_string(),
            "backspace" => "Backspace".to_string(),
            "tab" => "Tab".to_string(),
            other if other.chars().count() == 1 => other.to_uppercase(),
            other => {
                let mut chars = other.chars();
                match chars.next() {
                    // 功能键 f1..f12 → F1..F12；其余多字符键名首字母大写。
                    Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                    None => String::new(),
                }
            }
        })
        .collect();
    parts.join("+")
}

/// 校验用户覆盖表：未知动作名、非法键串、同动作重复项一律**忽略并告警**。
///
/// 返回「动作名 → 规范化键位」的干净覆盖表。宁可少生效一条覆盖，也不能因为用户
/// 写错一个键位就让程序起不来——这是本功能的硬底线。
fn validate_overrides(
    specs: &[ShortcutSpec],
    overrides: &BTreeMap<String, String>,
) -> BTreeMap<&'static str, String> {
    let mut accepted: BTreeMap<&'static str, String> = BTreeMap::new();
    for (raw_action, raw_key) in overrides {
        let Some(spec) = specs.iter().find(|spec| spec.action == raw_action.as_str()) else {
            tracing::warn!(action = %raw_action, "快捷键覆盖引用了未知动作名，已忽略");
            continue;
        };
        let key = normalize_key(raw_key);
        if !is_valid_key(&key) {
            tracing::warn!(action = %raw_action, key = %raw_key, "快捷键覆盖的键位无法解析，已忽略");
            continue;
        }
        if accepted.contains_key(spec.action) {
            tracing::warn!(action = %raw_action, "同一动作出现多条覆盖，只采用第一条");
            continue;
        }
        accepted.insert(spec.action, key);
    }
    accepted
}

/// 「内置默认表 + 用户覆盖」合成最终键位表（**纯函数**，可脱开 GPUI 单测）。
///
/// 规则：
/// - 覆盖按**动作名**定位（[`ShortcutSpec::action`]），只替换该动作的**主绑定**
///   （表里第一条）；同动作的其它默认绑定原样保留——改了「重做」的主键，
///   `Ctrl+Y` 仍在，「删除」的 `Backspace` 仍在。
/// - 未知动作名 / 非法键串在 [`validate_overrides`] 里被忽略并告警，不影响默认表。
/// - 冲突（同一条键被两个动作占用）时**保留默认的那条**：覆盖被驳回，该动作回退到
///   自己的默认键位；被驳回的覆盖键记进 [`ResolvedBinding::ignored_override`]，
///   并产出一条 [`Conflict`]。
/// - 没有覆盖时输出与 [`SHORTCUT_SPECS`] 逐条一致（键位、文案、顺序都不变）。
pub fn resolve_bindings(
    specs: &[ShortcutSpec],
    overrides: &BTreeMap<String, String>,
) -> (ResolvedBindings, Vec<Conflict>) {
    let accepted = validate_overrides(specs, overrides);

    let mut primary_seen: HashSet<&'static str> = HashSet::new();
    let mut candidates: Vec<Candidate> = Vec::with_capacity(specs.len());
    for (index, spec) in specs.iter().enumerate() {
        let overridden = if primary_seen.insert(spec.action) {
            accepted.get(spec.action)
        } else {
            None
        };
        let (key, is_override) = match overridden {
            Some(key) => (key.clone(), true),
            None => (spec.key.to_string(), false),
        };
        candidates.push(Candidate {
            index,
            action: spec.action,
            key,
            default_key: spec.key.to_string(),
            default_hint_key: spec.hint_key,
            hint: spec.hint,
            is_override,
        });
    }

    // 消解冲突：默认绑定 > 覆盖绑定；同为覆盖时取表里靠前的那条。
    // 默认键位彼此唯一，所以先让默认绑定全部占位，再处理覆盖——覆盖撞上任何已占
    // 键位就回退到自己的默认键位（动作仍可用），并记下这次被驳回的覆盖。
    let mut order: Vec<usize> = (0..candidates.len()).collect();
    order.sort_by_key(|&i| (candidates[i].is_override, candidates[i].index));

    let mut owner: HashMap<String, usize> = HashMap::new();
    let mut final_key: Vec<Option<String>> = candidates
        .iter()
        .map(|candidate| Some(candidate.key.clone()))
        .collect();
    let mut ignored_override: Vec<Option<String>> = vec![None; candidates.len()];
    let mut conflicts: Vec<Conflict> = Vec::new();

    for &i in &order {
        let Some(key) = final_key[i].clone() else {
            continue;
        };
        // 用 `insert` 的返回值一次拿到「之前是谁占着」，避免 contains_key + insert 的双重查找。
        let Some(holder) = owner.insert(key.clone(), i) else {
            // 这个键位本来是空的，占位成功。
            continue;
        };
        // 占位失败：`holder` 优先级一定不低于 `i`（默认在前、同优先级按表内先后），
        // 因此先把赢家放回去，再处理输家 `i`。
        owner.insert(key.clone(), holder);
        let winner = candidates[holder].action;
        let loser = candidates[i].action;
        let label = format_key_label(&key);
        if candidates[i].is_override {
            ignored_override[i] = Some(key.clone());
            let fallback = candidates[i].default_key.clone();
            match owner.insert(fallback.clone(), i) {
                // 默认键位空着 → 回退成功，动作仍有一个键可用。
                None => final_key[i] = Some(fallback),
                Some(occupier) => {
                    // 连默认键位都被别的覆盖抢先了（极罕见）→ 恢复占用者，这条绑定整个丢弃。
                    owner.insert(fallback, occupier);
                    final_key[i] = None;
                }
            }
            conflicts.push(Conflict {
                detail: format!(
                    "「{label}」被 {winner} 与 {loser} 同时占用：保留默认的 {winner}，忽略 {loser} 的覆盖"
                ),
                key,
                kept_action: winner,
                dropped_action: loser,
            });
        } else {
            // 默认表本身键位唯一，正常到不了这里；真到了就丢弃后到的那条，不静默串键。
            final_key[i] = None;
            conflicts.push(Conflict {
                detail: format!(
                    "「{label}」被 {winner} 与 {loser} 两个默认绑定同时占用：丢弃靠后的 {loser}"
                ),
                key,
                kept_action: winner,
                dropped_action: loser,
            });
        }
    }

    let mut bindings: Vec<ResolvedBinding> = Vec::with_capacity(candidates.len());
    for (i, candidate) in candidates.iter().enumerate() {
        let Some(key) = final_key[i].as_ref() else {
            continue;
        };
        // 只有「生效的覆盖」才用机械排版的键位文案；回退/默认一律用内置文案。
        let kept_override = candidate.is_override && ignored_override[i].is_none();
        let hint_key = if kept_override {
            format_key_label(key)
        } else {
            candidate.default_hint_key.to_string()
        };
        bindings.push(ResolvedBinding {
            action: candidate.action,
            key: key.clone(),
            hint_key,
            hint: candidate.hint,
            is_override: kept_override,
            ignored_override: ignored_override[i].clone(),
        });
    }
    (ResolvedBindings { bindings }, conflicts)
}

/// 注册全局键位：读取 `config.toml` 的 `[shortcuts] overrides`，与内置默认表合成。
///
/// 在 `Application::run` 里调用一次。配置读不到（文件损坏 / 权限不足）就退回默认
/// 键位——快捷键从来不该成为「程序起不来」的理由。
pub fn bind_default_keys(cx: &mut gpui::App) {
    let overrides = crate::utils::AppConfig::load_from_file("config.toml")
        .map(|config| config.shortcuts.overrides)
        .unwrap_or_default();
    bind_keys_with_overrides(cx, &overrides);
}

/// 按「默认表 + 用户覆盖」合成并注册全局键位（覆盖表由调用方提供，便于复用与测试）。
pub fn bind_keys_with_overrides(cx: &mut gpui::App, overrides: &BTreeMap<String, String>) {
    let (resolved, conflicts) = resolve_bindings(&SHORTCUT_SPECS, overrides);
    for conflict in &conflicts {
        tracing::warn!(
            key = %conflict.key,
            kept = %conflict.kept_action,
            dropped = %conflict.dropped_action,
            "{}",
            conflict.detail
        );
    }
    cx.bind_keys(
        resolved
            .bindings
            .iter()
            .filter_map(|binding| binding_from_action(binding.action, &binding.key)),
    );
}

/// 界面上的快捷键速查表。
///
/// 与 [`SHORTCUT_SPECS`]（真正的绑定表）同一份动作清单，逐条对齐由本文件末尾的
/// 测试守着——补了绑定却忘了写速查表、或速查表里留着已经删掉的键位，都会先在这里
/// 失败，而不是让用户对着界面上写错的键位按半天。
pub const SHORTCUT_HINTS: [(&str, &str); 16] = [
    ("Ctrl+Space", "播放/暂停"),
    ("Alt+↑↓", "上/下句"),
    ("Alt+←→", "±1 秒"),
    ("Ctrl+Enter", "开始转写"),
    ("Esc", "关闭/终止"),
    ("Ctrl+E", "导出"),
    ("Ctrl+F", "搜索"),
    ("Ctrl+K", "命令面板"),
    ("Ctrl+Z", "撤销"),
    ("Ctrl+Shift+Z", "重做"),
    ("Ctrl+Y", "重做"),
    ("Ctrl+H", "查找替换"),
    ("Space", "播放/暂停（裸键）"),
    ("Delete", "删除当前句（裸键）"),
    ("Backspace", "删除当前句（裸键）"),
    ("F1", "快捷键速查"),
];

/// 一组速查行：（分组名，该组里每行的（键位文本，说明））。
pub type ShortcutGroup = (&'static str, Vec<(&'static str, &'static str)>);

/// 速查面板里的分组顺序。
///
/// 十几条键位平铺成一列时，播放、编辑、导出混在一起，找一条得逐行扫；按「用户此刻
/// 想干什么」分组后，找键位变成先找组、再找行。顺序表放在这里而不是视图里，是为了
/// 让面板与测试共用同一份定义（测试会检查分组顺序、每组非空、没有重复分组）。
pub const SHORTCUT_GROUP_ORDER: [&str; 6] = [
    "播放与跳转",
    "编辑与撤销",
    "查找与面板",
    "文件与导出",
    "转写任务",
    "删除（裸键）",
];

/// 没登记分组的键位落到哪个分组。
///
/// 分组只是展示层信息，不是绑定的前置条件：将来加了新绑定却忘了登记分组时，它应该
/// 出现在面板的最后一块，而不是从面板里消失（那等于又造出一条「只有作者知道」的
/// 快捷键）。有这层兜底，漏登记最多是分组难看，不会丢信息。
pub const SHORTCUT_GROUP_OTHER: &str = "其它";

/// 一条键位描述属于哪个分组。
///
/// 按动作语义归类：`ctrl-space` 与裸 `space` 都是播放，归同一组——用户在面板里是拿
/// 「我想干什么」找键位，而不是拿键位反查动作，所以同义键位不该被拆到两个组里。
pub fn group_of(key: &str) -> &'static str {
    match SHORTCUT_SPECS.iter().find(|spec| spec.key == key) {
        Some(spec) => group_of_action(spec.action),
        // 未登记的键位描述兜底成「其它」而不是 panic（见 SHORTCUT_GROUP_OTHER）。
        None => SHORTCUT_GROUP_OTHER,
    }
}

/// 一个**动作名**属于哪个分组。
///
/// 覆盖后的键位描述不在 [`SHORTCUT_SPECS`] 里，`group_of(key)` 认不出它，因此合成
/// 结果的分组必须按动作名来分（见 [`resolved_shortcut_rows`]）。
pub fn group_of_action(action: &str) -> &'static str {
    match action {
        "toggle_play" | "toggle_play_bare" | "prev_segment" | "next_segment" | "seek_backward"
        | "seek_forward" => "播放与跳转",
        "undo" | "redo" => "编辑与撤销",
        "focus_subtitle_search"
        | "open_command_palette"
        | "open_replace_panel"
        | "open_shortcuts_help" => "查找与面板",
        "export_subtitle" => "文件与导出",
        "start_transcription" | "cancel_or_close" => "转写任务",
        "delete_active_segment" => "删除（裸键）",
        _ => SHORTCUT_GROUP_OTHER,
    }
}

/// 按分组整理好的速查行，供快捷键速查面板直接渲染。
///
/// 直接由 [`SHORTCUT_SPECS`] 推导，而不是手写第二份清单——手写那份迟早与真正注册的
/// 键位对不上（这个坑在 [`SHORTCUT_HINTS`] 上已经踩过一次）。同一分组里键位文本相同
/// 的条目只留一条（`Alt+↑↓` 是两条绑定共用一行文案），键位不同、说明相同则各自保留
/// （`Delete` 与 `Backspace` 都删当前句，但用户需要知道自己能按哪个键）。
///
/// 分组按 [`SHORTCUT_GROUP_ORDER`] 的顺序返回，空分组不返回（面板上挂一个空标题会
/// 让人以为那组键位丢了）。
pub fn grouped_shortcut_rows() -> Vec<ShortcutGroup> {
    let mut groups: Vec<ShortcutGroup> = SHORTCUT_GROUP_ORDER
        .iter()
        .map(|name| (*name, Vec::new()))
        // 兜底组固定在表尾，`group_of` 的默认返回值一定落在它身上。
        .chain(std::iter::once((SHORTCUT_GROUP_OTHER, Vec::new())))
        .collect();
    for spec in SHORTCUT_SPECS {
        let index = groups
            .iter()
            .position(|(name, _)| *name == group_of(spec.key))
            // `unwrap_or` 的参数在这里求值时还没有可变借用，取表尾当兜底组。
            .unwrap_or(groups.len() - 1);
        let rows = &mut groups[index].1;
        if rows.iter().any(|(key, _)| *key == spec.hint_key) {
            continue;
        }
        rows.push((spec.hint_key, spec.hint));
    }
    groups.retain(|(_, rows)| !rows.is_empty());
    groups
}

/// 速查面板里的一组行（键位文本可来自用户覆盖，所以是 `String`）。
pub type ResolvedShortcutGroup = (String, Vec<(String, String)>);

/// 按分组整理好的速查行——**基于合成结果**，用户改过的键位会如实显示。
///
/// 与 [`grouped_shortcut_rows`] 的关系：后者永远按内置默认表渲染（键位文本是
/// `&'static str`），本函数按 [`resolve_bindings`] 的结果渲染。速查面板要反映
/// 用户覆盖时，把渲染源从前者换成后者即可，**分组顺序与去重规则完全一致**。
/// 每条被冲突驳回的覆盖会在说明后追加一句标注（[`ResolvedBinding::ignored_override`]）。
pub fn resolved_shortcut_rows(resolved: &ResolvedBindings) -> Vec<ResolvedShortcutGroup> {
    let mut groups: Vec<ResolvedShortcutGroup> = SHORTCUT_GROUP_ORDER
        .iter()
        .map(|name| ((*name).to_string(), Vec::new()))
        // 兜底组固定在表尾，`group_of_action` 的默认返回值一定落在它身上。
        .chain(std::iter::once((
            SHORTCUT_GROUP_OTHER.to_string(),
            Vec::new(),
        )))
        .collect();
    for binding in &resolved.bindings {
        // 分组按**动作名**归类：键位被覆盖后 `group_of(key)` 认不出它了。
        let group = group_of_action(binding.action);
        let index = groups
            .iter()
            .position(|(name, _)| name.as_str() == group)
            .unwrap_or(groups.len() - 1);
        let rows = &mut groups[index].1;
        if rows.iter().any(|(key, _)| *key == binding.hint_key) {
            continue;
        }
        let mut hint = binding.hint.to_string();
        if binding.ignored_override.is_some() {
            hint.push_str("（键位覆盖与其他命令冲突，已忽略）");
        }
        rows.push((binding.hint_key.clone(), hint));
    }
    groups.retain(|(_, rows)| !rows.is_empty());
    groups
}

#[cfg(test)]
mod tests {
    use super::{
        binding_for, group_of, grouped_shortcut_rows, resolve_bindings, resolved_shortcut_rows,
        DeleteActiveSegment, OpenShortcutsHelp, ResolvedBindings, TogglePlaybackBare, BARE_KEYS,
        SHORTCUT_GROUP_ORDER, SHORTCUT_GROUP_OTHER, SHORTCUT_HINTS, SHORTCUT_SPECS, SPEC_COUNT,
    };
    use std::collections::BTreeMap;

    /// 速查表必须覆盖每一条实际绑定：漏掉 Ctrl+Y 这类键位时，用户永远不知道
    /// 它存在（历史上正是这么漏过一次）。
    #[test]
    fn hints_cover_every_binding() {
        for spec in SHORTCUT_SPECS {
            assert!(
                SHORTCUT_HINTS
                    .iter()
                    .any(|(key, hint)| *key == spec.hint_key && *hint == spec.hint),
                "绑定 {} 在速查表里没有对应条目",
                spec.key
            );
        }
    }

    /// 反向：速查表里不能留着没有任何绑定的条目（写完却按不出来）。
    #[test]
    fn every_hint_has_a_binding() {
        for (key, _) in SHORTCUT_HINTS {
            assert!(
                SHORTCUT_SPECS.iter().any(|spec| spec.hint_key == key),
                "速查表条目 {key} 没有任何绑定"
            );
        }
    }

    /// 每条键位描述都必须能被解析：拼错键位名（如 `ctrl-spacebar`）会在这里炸，
    /// 而不是等到运行时 `KeyBinding::new` 内部 panic。
    #[test]
    fn every_spec_resolves_to_a_binding() {
        for spec in SHORTCUT_SPECS {
            let _ = binding_for(spec.key);
        }
    }

    /// 速查表键位不重复：同一键位写两行会让人以为绑了两个功能。
    #[test]
    fn shortcut_hint_keys_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for (key, _) in SHORTCUT_HINTS {
            assert!(seen.insert(key), "速查表里重复的键位: {key}");
        }
    }

    /// 未登记的键位描述不应静默降级成别的动作。
    #[test]
    #[should_panic]
    fn unknown_key_spec_is_rejected() {
        let _ = binding_for("ctrl-shift-alt-q");
    }

    /// 裸键最小交互集必须真的注册进键位表，且键名就是 GPUI 认的那几个
    /// （拼成 `spacebar` / `del` 时 `binding_for` 会 panic，这里先按名字断言一次）。
    #[test]
    fn bare_keys_are_registered_with_expected_names() {
        assert!(BARE_KEYS.contains(&"space"), "裸键表里应有 space");
        assert!(BARE_KEYS.contains(&"delete"), "裸键表里应有 delete");
        assert!(BARE_KEYS.contains(&"backspace"), "裸键表里应有 backspace");
        for key in BARE_KEYS {
            assert!(
                SHORTCUT_SPECS.iter().any(|spec| spec.key == key),
                "裸键 {key} 没有注册进 SHORTCUT_SPECS"
            );
            // 每条裸键都要能被解析成真正的 KeyBinding（键名拼错会在这里炸）。
            let _ = binding_for(key);
        }
        // 速查表也要能看到裸键，否则用户永远不知道空格 / Delete 能用。
        assert!(SHORTCUT_HINTS.iter().any(|(key, _)| *key == "Space"));
        assert!(SHORTCUT_HINTS.iter().any(|(key, _)| *key == "Delete"));
        assert!(SHORTCUT_HINTS.iter().any(|(key, _)| *key == "Backspace"));
    }

    /// 裸键不能带修饰键（写成 `ctrl-space` 就退回组合键了），
    /// 也不能和既有快捷键撞成同一条键位描述。
    #[test]
    fn bare_keys_do_not_collide_with_modified_bindings() {
        for key in BARE_KEYS {
            assert!(
                !key.contains('-'),
                "裸键 {key} 带了修饰键，这里应该是不含 '-' 的键名"
            );
        }
        // 全表键位描述唯一：裸 `space` 与 `ctrl-space` 是两条不同绑定，各自保留。
        let mut seen = std::collections::HashSet::new();
        for spec in SHORTCUT_SPECS {
            assert!(seen.insert(spec.key), "重复的键位描述: {}", spec.key);
        }
        assert!(seen.contains("space"), "裸 space 绑定丢了");
        assert!(seen.contains("ctrl-space"), "原有 Ctrl+Space 绑定丢了");
        assert!(seen.contains("delete"), "裸 delete 绑定丢了");
        assert!(seen.contains("backspace"), "裸 backspace 绑定丢了");
    }

    /// 每条裸键都必须落在预期的动作上：空格 = 播放/暂停，Delete 与 Backspace 同源 = 删除当前句。
    /// 只断言「能解析」不够——把 backspace 误接到 `TogglePlaybackBare` 上同样能解析通过，
    /// 这里按动作名逐条对齐，改错接线会立刻失败。
    #[test]
    fn bare_keys_map_to_expected_actions() {
        use gpui::Action as _;

        let space = binding_for("space").action().name();
        let delete = binding_for("delete").action().name();
        let backspace = binding_for("backspace").action().name();

        assert_eq!(
            space,
            TogglePlaybackBare::name_for_type(),
            "裸空格应绑定「播放/暂停」"
        );
        assert_eq!(
            delete,
            DeleteActiveSegment::name_for_type(),
            "裸 Delete 应绑定「删除当前句」"
        );
        assert_eq!(
            backspace,
            DeleteActiveSegment::name_for_type(),
            "裸 Backspace 应与 Delete 同源（删除当前句）"
        );
        assert_ne!(space, delete, "空格与删除键不应落到同一个动作上");
    }

    /// F1 必须真的落在「快捷键速查」动作上：面板入口只写在这一条绑定上，接错线
    /// （比如并到 Esc 上）面板就再也打不开，所以这里按动作名钉死，不只看「能解析」。
    #[test]
    fn f1_opens_shortcuts_help() {
        use gpui::Action as _;

        assert_eq!(
            binding_for("f1").action().name(),
            OpenShortcutsHelp::name_for_type(),
            "F1 应绑定「快捷键速查」"
        );
        assert!(
            SHORTCUT_HINTS
                .iter()
                .any(|(key, hint)| *key == "F1" && *hint == "快捷键速查"),
            "速查表里应有 F1 条目"
        );
        // F1 不落在打字路径上，没有「焦点在输入框就失效」的语义，不该混进裸键集。
        assert!(!BARE_KEYS.contains(&"f1"), "F1 不该出现在裸键表里");
    }

    /// 每个键位都必须显式登记分组：漏登记时 `group_of` 会兜底到「其它」，
    /// 面板上多出一块无名分组，这里直接拦下来。
    #[test]
    fn every_spec_has_a_named_group() {
        for spec in SHORTCUT_SPECS {
            let group = group_of(spec.key);
            assert_ne!(
                group, SHORTCUT_GROUP_OTHER,
                "键位 {} 没有登记到任何分组",
                spec.key
            );
            assert!(
                SHORTCUT_GROUP_ORDER.contains(&group),
                "键位 {} 落到了未登记的分组 {group}",
                spec.key
            );
        }
    }

    /// 未登记的键位描述兜底成「其它」而不是 panic：分组是展示层信息，将来加绑定
    /// 忘了登记时仍要能在面板上看到它。
    #[test]
    fn unknown_key_falls_back_to_other_group() {
        assert_eq!(group_of("ctrl-alt-shift-f13"), SHORTCUT_GROUP_OTHER);
    }

    /// 面板内容必须与速查表逐条一致（去重后比较）：两条绑定共用一行文案
    /// （`Alt+↑↓`）时只该显示一行，但也不能因此少显示别的条目。
    #[test]
    fn grouped_rows_mirror_hints_table() {
        let mut grouped: Vec<(&str, &str)> = grouped_shortcut_rows()
            .into_iter()
            .flat_map(|(_, rows)| rows)
            .collect();
        grouped.sort_unstable();
        grouped.dedup();

        let mut expected: Vec<(&str, &str)> = SHORTCUT_HINTS.to_vec();
        expected.sort_unstable();
        expected.dedup();

        assert_eq!(grouped, expected, "面板内容与速查表不一致");
    }

    /// 分组按 [`SHORTCUT_GROUP_ORDER`] 的顺序出现、且没有空分组：顺序错乱会让用户
    /// 找不到键位，空标题会让人以为那组键位丢了。
    #[test]
    fn groups_are_ordered_and_nonempty() {
        let rows = grouped_shortcut_rows();
        assert!(!rows.is_empty(), "速查面板一行都没有");

        let mut seen: Vec<&str> = Vec::new();
        for (name, entries) in &rows {
            assert!(!entries.is_empty(), "分组 {name} 是空的");
            assert!(!seen.contains(name), "分组 {name} 出现了两次");
            seen.push(name);
        }
        // 出现的分组必须保持分组表的相对顺序（「其它」兜底组固定在表尾）。
        let positions: Vec<usize> = seen
            .iter()
            .map(|name| {
                SHORTCUT_GROUP_ORDER
                    .iter()
                    .position(|candidate| candidate == name)
                    .unwrap_or(SHORTCUT_GROUP_ORDER.len())
            })
            .collect();
        let mut sorted = positions.clone();
        sorted.sort_unstable();
        assert_eq!(positions, sorted, "分组顺序与 SHORTCUT_GROUP_ORDER 不一致");
    }

    /// 定长数组的 N 与常量必须一致：改了表却忘了改 N 时，这里会先失败
    /// （N 被写大时多余项会退化成默认值，键位描述为空，同样是错的）。
    #[test]
    fn spec_count_matches_array_length() {
        assert_eq!(SHORTCUT_SPECS.len(), SPEC_COUNT);
        assert_eq!(SPEC_COUNT, 18, "新增绑定后请同步更新 SPEC_COUNT");
        // 每条裸键都恰好出现一次，不多不少。
        for key in BARE_KEYS {
            let count = SHORTCUT_SPECS.iter().filter(|spec| spec.key == key).count();
            assert_eq!(count, 1, "裸键 {key} 在键位表里出现了 {count} 次");
        }
    }

    // ─────────── 键位覆盖合成（resolve_bindings） ───────────

    /// 覆盖表构造助手：`[("动作名", "键位")]` → `BTreeMap`。
    fn overrides(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(action, key)| ((*action).to_string(), (*key).to_string()))
            .collect()
    }

    /// 某个动作名在合成结果里的**主绑定**键位（第一条）。
    fn resolved_key_for<'a>(resolved: &'a ResolvedBindings, action: &str) -> Option<&'a str> {
        resolved
            .bindings
            .iter()
            .find(|binding| binding.action == action)
            .map(|binding| binding.key.as_str())
    }

    /// 没有任何覆盖时，合成结果必须与内置默认表**逐条一致**：键位、顺序、速查文案
    /// 一个字节都不变（这是「默认行为不变」的硬约束）。
    #[test]
    fn resolve_without_overrides_matches_default_table() {
        let (resolved, conflicts) = resolve_bindings(&SHORTCUT_SPECS, &BTreeMap::new());
        assert!(conflicts.is_empty(), "没有覆盖时不该报冲突");
        assert_eq!(resolved.bindings.len(), SHORTCUT_SPECS.len());
        for (binding, spec) in resolved.bindings.iter().zip(SHORTCUT_SPECS.iter()) {
            assert_eq!(binding.action, spec.action);
            assert_eq!(binding.key, spec.key, "默认键位不能变");
            assert_eq!(binding.hint_key, spec.hint_key, "速查文案不能变");
            assert_eq!(binding.hint, spec.hint);
            assert!(!binding.is_override);
            assert!(binding.ignored_override.is_none());
        }
    }

    /// 覆盖生效：主绑定换成新键、文案跟着重排，其它动作的键位一个都不动。
    #[test]
    fn override_replaces_the_primary_binding() {
        let ov = overrides(&[("toggle_play", "ctrl-shift-p")]);
        let (resolved, conflicts) = resolve_bindings(&SHORTCUT_SPECS, &ov);
        assert!(conflicts.is_empty(), "无冲突的单条覆盖不该报冲突");
        let toggle = resolved
            .bindings
            .iter()
            .find(|binding| binding.action == "toggle_play")
            .expect("toggle_play 一定在结果里");
        assert_eq!(toggle.key, "ctrl-shift-p");
        assert_eq!(toggle.hint_key, "Ctrl+Shift+P", "覆盖键要重排成可读文案");
        assert!(toggle.is_override);
        assert!(toggle.ignored_override.is_none());
        assert_eq!(
            resolved_key_for(&resolved, "open_command_palette"),
            Some("ctrl-k"),
            "别的动作不受影响"
        );
    }

    /// 未知动作名被忽略并告警（这里只断言「被忽略」）：默认表原样生效，不 panic。
    #[test]
    fn unknown_action_name_is_ignored() {
        let ov = overrides(&[("not_a_real_action", "ctrl-u")]);
        let (resolved, conflicts) = resolve_bindings(&SHORTCUT_SPECS, &ov);
        assert!(conflicts.is_empty());
        assert_eq!(resolved.bindings.len(), SHORTCUT_SPECS.len());
        assert!(resolved.bindings.iter().all(|binding| !binding.is_override));
        assert!(
            !resolved
                .bindings
                .iter()
                .any(|binding| binding.key == "ctrl-u"),
            "未知动作带来的键位不该被注册"
        );
    }

    /// 非法键串被忽略：空串、纯空白、多键序列、解析失败（`ctrl-k-j`）都不能把程序
    /// 拖崩，对应动作仍用默认键位。
    #[test]
    fn invalid_key_strings_are_ignored() {
        for bad in ["", "   ", "ctrl-k ctrl-j", "ctrl-k-j"] {
            let ov = overrides(&[("toggle_play", bad)]);
            let (resolved, conflicts) = resolve_bindings(&SHORTCUT_SPECS, &ov);
            assert!(conflicts.is_empty(), "非法键位不该算冲突: {bad:?}");
            assert_eq!(
                resolved_key_for(&resolved, "toggle_play"),
                Some("ctrl-space"),
                "非法覆盖 {bad:?} 必须回落到默认键位"
            );
        }
    }

    /// 冲突：覆盖撞上别人**默认**占用的键位时，保留默认的那条，覆盖回退默认键，
    /// 并把这次被驳回的覆盖记进 `ignored_override`（供速查表标注）。
    #[test]
    fn conflicting_override_loses_to_the_default_binding() {
        let ov = overrides(&[("export_subtitle", "ctrl-space")]);
        let (resolved, conflicts) = resolve_bindings(&SHORTCUT_SPECS, &ov);
        assert_eq!(conflicts.len(), 1, "应当恰好报出一处冲突");
        assert_eq!(conflicts[0].key, "ctrl-space");
        assert_eq!(conflicts[0].kept_action, "toggle_play", "默认占用者胜出");
        assert_eq!(conflicts[0].dropped_action, "export_subtitle");

        let export = resolved
            .bindings
            .iter()
            .find(|binding| binding.action == "export_subtitle")
            .expect("导出一定在结果里");
        assert_eq!(export.key, "ctrl-e", "被驳回的覆盖回退到默认键位");
        assert!(!export.is_override);
        assert_eq!(export.ignored_override.as_deref(), Some("ctrl-space"));
        assert_eq!(
            resolved_key_for(&resolved, "toggle_play"),
            Some("ctrl-space"),
            "默认占用者原样保留"
        );
    }

    /// 两个覆盖抢同一个键：表里靠前的那条胜出，另一条回退自己的默认键。
    #[test]
    fn earlier_override_wins_an_override_vs_override_conflict() {
        let ov = overrides(&[("undo", "ctrl-p"), ("redo", "ctrl-p")]);
        let (resolved, conflicts) = resolve_bindings(&SHORTCUT_SPECS, &ov);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].kept_action, "undo", "靠前的动作胜出");
        assert_eq!(conflicts[0].dropped_action, "redo");
        assert_eq!(resolved_key_for(&resolved, "undo"), Some("ctrl-p"));
        assert_eq!(
            resolved_key_for(&resolved, "redo"),
            Some("ctrl-shift-z"),
            "重做回退到默认主键"
        );
    }

    /// 覆盖只换主绑定：同动作的备用默认绑定（重做的 Ctrl+Y）保留。
    #[test]
    fn override_keeps_the_secondary_default_binding() {
        let ov = overrides(&[("redo", "ctrl-r")]);
        let (resolved, conflicts) = resolve_bindings(&SHORTCUT_SPECS, &ov);
        assert!(conflicts.is_empty());
        let redo_keys: Vec<&str> = resolved
            .bindings
            .iter()
            .filter(|binding| binding.action == "redo")
            .map(|binding| binding.key.as_str())
            .collect();
        assert_eq!(
            redo_keys,
            vec!["ctrl-r", "ctrl-y"],
            "主键被覆盖，备用键保留"
        );
    }

    /// 速查行按**合成结果**渲染：覆盖键位出现在对应分组里，被驳回的覆盖带标注。
    #[test]
    fn resolved_rows_reflect_overrides_and_mark_conflicts() {
        let ov = overrides(&[
            ("toggle_play", "ctrl-shift-p"),
            // Ctrl+K 是「命令面板」的默认键位且未被覆盖，这条覆盖会被驳回。
            ("export_subtitle", "ctrl-k"),
        ]);
        let (resolved, _) = resolve_bindings(&SHORTCUT_SPECS, &ov);
        let rows = resolved_shortcut_rows(&resolved);

        let playback = rows
            .iter()
            .find(|(name, _)| name == "播放与跳转")
            .expect("应有播放分组");
        assert!(
            playback
                .1
                .iter()
                .any(|(key, hint)| key == "Ctrl+Shift+P" && hint == "播放/暂停"),
            "覆盖后的播放键位应出现在播放分组里"
        );

        let files = rows
            .iter()
            .find(|(name, _)| name == "文件与导出")
            .expect("应有导出分组");
        assert!(
            files
                .1
                .iter()
                .any(|(key, hint)| key == "Ctrl+E" && hint.contains("冲突")),
            "被驳回的覆盖要在速查表里标注"
        );
    }

    /// README 的「快捷键」一节必须与 [`SHORTCUT_SPECS`] 同源。
    ///
    /// F1 速查面板是从这张表渲染的，所以面板永远不会错；README 是手写的，改了键位
    /// 表却忘了改 README，文档就会教用户按一个不再生效的键——历史上 README 就写过
    /// 一条从未绑定过的 `Ctrl+T 切换主题`。这里直接读仓库根的 README，把每条
    /// `hint_key` 是否出现钉住，键位增删时文档不同步会立刻失败。
    #[test]
    fn readme_shortcut_table_covers_every_spec() {
        let readme = include_str!("../../README.md");
        for spec in SHORTCUT_SPECS.iter() {
            assert!(
                readme.contains(spec.hint_key),
                "README 缺少快捷键 {}（{}）",
                spec.hint_key,
                spec.hint
            );
        }
    }
}
