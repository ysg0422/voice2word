//! 全局快捷键 (F-017)
//!
//! 设计取舍：**修饰键组合负责全局生效；裸键只保留「焦点不在输入框时才生效」的最小交互集**。
//!
//! 主界面的字幕搜索框、在线翻译配置框都是 GPUI 自绘输入（见 `apply_line_edit`），
//! 靠 `on_key_down` 直接插入字符。键位表是全局的，命中动作后字符就不再进入输入框，
//! 因此裸键绑定（空格 = 播放/暂停、Delete = 删除当前句）与打字天然冲突。这里的折中是：
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

use gpui::{actions, KeyBinding};

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
        /// 切换深浅主题
        ToggleTheme,
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
        /// 裸空格（焦点不在输入框时）：播放 / 暂停当前预览
        TogglePlaybackBare,
        /// 裸 Delete（焦点不在输入框时）：删除当前选中的字幕片段
        DeleteActiveSegment,
    ]
);

/// 一条快捷键的唯一定义。
///
/// `key` 是传给 [`KeyBinding::new`] 的键位描述，同时也驱动 [`binding_for`] 的
/// 分派；`hint_key` / `hint` 是速查表 [`SHORTCUT_HINTS`] 里的展示文本。
/// 展示文本与实际绑定放在同一张表里，两条绑定也可以共用同一行文案
/// （`Alt+↑↓` 就覆盖了上下两个方向键），因此不存在「速查表和实际按键对不上」。
pub struct ShortcutSpec {
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
pub const BARE_KEYS: [&str; 2] = ["space", "delete"];

/// [`SHORTCUT_SPECS`] 的条目数：定长数组与测试共用同一个常量，
/// 以后加绑定只改这一处，不会再出现「表里加了、长度常量忘了改」。
pub const SPEC_COUNT: usize = 16;

/// 默认键位表的唯一事实来源：启动时按此注册，速查表也按此校验。
pub const SHORTCUT_SPECS: [ShortcutSpec; SPEC_COUNT] = [
    ShortcutSpec {
        key: "ctrl-space",
        hint_key: "Ctrl+Space",
        hint: "播放/暂停",
    },
    ShortcutSpec {
        key: "alt-up",
        hint_key: "Alt+↑↓",
        hint: "上/下句",
    },
    ShortcutSpec {
        key: "alt-down",
        hint_key: "Alt+↑↓",
        hint: "上/下句",
    },
    ShortcutSpec {
        key: "alt-left",
        hint_key: "Alt+←→",
        hint: "±1 秒",
    },
    ShortcutSpec {
        key: "alt-right",
        hint_key: "Alt+←→",
        hint: "±1 秒",
    },
    ShortcutSpec {
        key: "ctrl-enter",
        hint_key: "Ctrl+Enter",
        hint: "开始转写",
    },
    ShortcutSpec {
        key: "escape",
        hint_key: "Esc",
        hint: "关闭/终止",
    },
    ShortcutSpec {
        key: "ctrl-e",
        hint_key: "Ctrl+E",
        hint: "导出",
    },
    ShortcutSpec {
        key: "ctrl-f",
        hint_key: "Ctrl+F",
        hint: "搜索",
    },
    ShortcutSpec {
        key: "ctrl-k",
        hint_key: "Ctrl+K",
        hint: "命令面板",
    },
    ShortcutSpec {
        key: "ctrl-z",
        hint_key: "Ctrl+Z",
        hint: "撤销",
    },
    ShortcutSpec {
        key: "ctrl-shift-z",
        hint_key: "Ctrl+Shift+Z",
        hint: "重做",
    },
    // Windows 用户习惯用 Ctrl+Y 重做，而浏览器/编辑器过来的用户习惯 Ctrl+Shift+Z，
    // 两套都留着才不别扭。键位描述各不相同，但速查表里各占一行，用户两套都能看到。
    ShortcutSpec {
        key: "ctrl-y",
        hint_key: "Ctrl+Y",
        hint: "重做",
    },
    // Ctrl+H 是「查找替换」的通用约定（VS Code / Word / 浏览器都是它）。
    // 搜索已经占了 Ctrl+F，替换若没有独立键位，用户只能靠鼠标去点那个小按钮。
    ShortcutSpec {
        key: "ctrl-h",
        hint_key: "Ctrl+H",
        hint: "查找替换",
    },
    // 裸键最小交互集：只在焦点不在输入框时由 `.on_action` 放行（见 `BARE_KEYS`）。
    // 空格是播放/暂停的通用约定（播放器、剪辑软件都是它），与 Ctrl+Space 并存：
    // 前者给「手在鼠标上」的场景，后者给「正在打字」的场景，两条互不覆盖。
    ShortcutSpec {
        key: "space",
        hint_key: "Space",
        hint: "播放/暂停(裸键)",
    },
    // Delete 删除当前选中句，也是剪辑台里最顺手的一键操作。
    ShortcutSpec {
        key: "delete",
        hint_key: "Delete",
        hint: "删除当前句(裸键)",
    },
];

/// 把一条键位描述解析成具体的 [`KeyBinding`]。
///
/// 用 `match` 而不是让调用点逐个手写 `KeyBinding::new`，是为了让「有哪些绑定」
/// 只由 [`SHORTCUT_SPECS`] 决定：新增一条绑定只需改表 + 在这里加一个分支，
/// 速查表校验测试会在漏掉任一边时报错。
fn binding_for(key: &str) -> KeyBinding {
    match key {
        "ctrl-space" => KeyBinding::new(key, TogglePlayback, None),
        "alt-up" => KeyBinding::new(key, PrevSegment, None),
        "alt-down" => KeyBinding::new(key, NextSegment, None),
        "alt-left" => KeyBinding::new(key, SeekBackward, None),
        "alt-right" => KeyBinding::new(key, SeekForward, None),
        "ctrl-enter" => KeyBinding::new(key, StartTranscription, None),
        "escape" => KeyBinding::new(key, CancelOrClose, None),
        "ctrl-e" => KeyBinding::new(key, ExportSubtitle, None),
        "ctrl-f" => KeyBinding::new(key, FocusSubtitleSearch, None),
        "ctrl-k" => KeyBinding::new(key, OpenCommandPalette, None),
        "ctrl-z" => KeyBinding::new(key, Undo, None),
        "ctrl-shift-z" => KeyBinding::new(key, Redo, None),
        "ctrl-y" => KeyBinding::new(key, Redo, None),
        "ctrl-h" => KeyBinding::new(key, OpenReplacePanel, None),
        // 裸键：与上面的修饰键组合是两条独立绑定，键位描述不会互相覆盖。
        "space" => KeyBinding::new(key, TogglePlaybackBare, None),
        "delete" => KeyBinding::new(key, DeleteActiveSegment, None),
        other => panic!("SHORTCUT_SPECS 里有未登记的键位描述: {other}"),
    }
}

/// 注册默认键位表。在 `Application::run` 里调用一次，键位是全局的。
pub fn bind_default_keys(cx: &mut gpui::App) {
    cx.bind_keys(SHORTCUT_SPECS.iter().map(|spec| binding_for(spec.key)));
}

/// 界面上的快捷键速查表。
///
/// 与 [`SHORTCUT_SPECS`]（真正的绑定表）同一份动作清单，逐条对齐由本文件末尾的
/// 测试守着——补了绑定却忘了写速查表、或速查表里留着已经删掉的键位，都会先在这里
/// 失败，而不是让用户对着界面上写错的键位按半天。
pub const SHORTCUT_HINTS: [(&str, &str); 14] = [
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
    ("Space", "播放/暂停(裸键)"),
    ("Delete", "删除当前句(裸键)"),
];

#[cfg(test)]
mod tests {
    use super::{binding_for, BARE_KEYS, SHORTCUT_HINTS, SHORTCUT_SPECS, SPEC_COUNT};

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
    }

    /// 定长数组的 N 与常量必须一致：改了表却忘了改 N 时，这里会先失败
    /// （N 被写大时多余项会退化成默认值，键位描述为空，同样是错的）。
    #[test]
    fn spec_count_matches_array_length() {
        assert_eq!(SHORTCUT_SPECS.len(), SPEC_COUNT);
        assert_eq!(SPEC_COUNT, 16, "新增绑定后请同步更新 SPEC_COUNT");
        // 每条裸键都恰好出现一次，不多不少。
        for key in BARE_KEYS {
            let count = SHORTCUT_SPECS.iter().filter(|spec| spec.key == key).count();
            assert_eq!(count, 1, "裸键 {key} 在键位表里出现了 {count} 次");
        }
    }
}
