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
pub const BARE_KEYS: [&str; 3] = ["space", "delete", "backspace"];

/// [`SHORTCUT_SPECS`] 的条目数：定长数组与测试共用同一个常量，
/// 以后加绑定只改这一处，不会再出现「表里加了、长度常量忘了改」。
pub const SPEC_COUNT: usize = 18;

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
    // F1 是「帮助」的通用约定（浏览器、Office、各类编辑器都是它），而速查面板只做
    // 展示、不碰任何数据，可以全局安全生效；绑成 Ctrl+? 之类的组合反而没人会按。
    ShortcutSpec {
        key: "f1",
        hint_key: "F1",
        hint: "快捷键速查",
    },
    // 裸键最小交互集：只在焦点不在输入框时由 `.on_action` 放行（见 `BARE_KEYS`）。
    // 空格是播放/暂停的通用约定（播放器、剪辑软件都是它），与 Ctrl+Space 并存：
    // 前者给「手在鼠标上」的场景，后者给「正在打字」的场景，两条互不覆盖。
    ShortcutSpec {
        key: "space",
        hint_key: "Space",
        hint: "播放/暂停（裸键）",
    },
    // Delete 删除当前选中句，也是剪辑台里最顺手的一键操作。
    ShortcutSpec {
        key: "delete",
        hint_key: "Delete",
        hint: "删除当前句（裸键）",
    },
    // Backspace 与 Delete 同源：没有独立 Delete 键的键盘上，退格才是唯一顺手的删除键。
    // 两条裸键绑定同一个动作（`DeleteActiveSegment`），焦点在输入框时都由 `.on_action` 放行。
    ShortcutSpec {
        key: "backspace",
        hint_key: "Backspace",
        hint: "删除当前句（裸键）",
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
        "f1" => KeyBinding::new(key, OpenShortcutsHelp, None),
        // 裸键：与上面的修饰键组合是两条独立绑定，键位描述不会互相覆盖。
        "space" => KeyBinding::new(key, TogglePlaybackBare, None),
        "delete" => KeyBinding::new(key, DeleteActiveSegment, None),
        // Backspace 与 Delete 同源：键位描述不同，动作相同。
        "backspace" => KeyBinding::new(key, DeleteActiveSegment, None),
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
    match key {
        "ctrl-space" | "space" | "alt-up" | "alt-down" | "alt-left" | "alt-right" => "播放与跳转",
        "ctrl-z" | "ctrl-shift-z" | "ctrl-y" => "编辑与撤销",
        "ctrl-f" | "ctrl-k" | "ctrl-h" | "f1" => "查找与面板",
        "ctrl-e" => "文件与导出",
        "ctrl-enter" | "escape" => "转写任务",
        "delete" | "backspace" => "删除（裸键）",
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

#[cfg(test)]
mod tests {
    use super::{
        binding_for, group_of, grouped_shortcut_rows, DeleteActiveSegment, OpenShortcutsHelp,
        TogglePlaybackBare, BARE_KEYS, SHORTCUT_GROUP_ORDER, SHORTCUT_GROUP_OTHER, SHORTCUT_HINTS,
        SHORTCUT_SPECS, SPEC_COUNT,
    };

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
}
