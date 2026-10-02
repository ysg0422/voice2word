//! 全局快捷键 (F-017)
//!
//! 设计取舍：**全部采用「修饰键 + 键」的组合，不占用裸字符键**。
//!
//! 主界面的字幕搜索框、在线翻译配置框都是 GPUI 自绘输入（见 `apply_line_edit`），
//! 靠 `on_key_down` 直接插入字符。裸键绑定（例如把空格绑成播放/暂停）会与打字
//! 直接冲突——键位表是全局的，命中动作后字符就不再进入输入框。修饰键组合则不会
//! 出现在正常打字路径上，因此可以全局安全生效。
//!
//! 绑定集中在 [`bind_default_keys`]（启动时注册一次），动作处理散落在各视图的
//! `.on_action(...)` 上，避免把业务逻辑堆进这个模块。

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
    ]
);

/// 注册默认键位表。在 `Application::run` 里调用一次，键位是全局的。
pub fn bind_default_keys(cx: &mut gpui::App) {
    cx.bind_keys([
        KeyBinding::new("ctrl-space", TogglePlayback, None),
        KeyBinding::new("alt-up", PrevSegment, None),
        KeyBinding::new("alt-down", NextSegment, None),
        KeyBinding::new("alt-left", SeekBackward, None),
        KeyBinding::new("alt-right", SeekForward, None),
        KeyBinding::new("ctrl-enter", StartTranscription, None),
        KeyBinding::new("escape", CancelOrClose, None),
        KeyBinding::new("ctrl-e", ExportSubtitle, None),
        KeyBinding::new("ctrl-t", ToggleTheme, None),
        KeyBinding::new("ctrl-f", FocusSubtitleSearch, None),
        // 撤销/重做同时绑定两套键位：Windows 用户习惯 Ctrl+Y 重做，
        // 而从浏览器/编辑器过来的用户习惯 Ctrl+Shift+Z，两套都留着才不别扭。
        KeyBinding::new("ctrl-z", Undo, None),
        KeyBinding::new("ctrl-shift-z", Redo, None),
        KeyBinding::new("ctrl-y", Redo, None),
    ]);
}

/// 界面上的快捷键速查表。与 [`bind_default_keys`] 同源维护，
/// 改键位时两处一起改，避免界面上写着按 A 实际要按 B。
pub const SHORTCUT_HINTS: [(&str, &str); 10] = [
    ("Ctrl+Space", "播放/暂停"),
    ("Alt+↑↓", "上/下句"),
    ("Alt+←→", "±1 秒"),
    ("Ctrl+Enter", "开始转写"),
    ("Ctrl+E", "导出"),
    ("Ctrl+F", "搜索"),
    ("Ctrl+T", "主题"),
    ("Ctrl+Z", "撤销"),
    ("Ctrl+Shift+Z", "重做"),
    ("Esc", "关闭/终止"),
];
