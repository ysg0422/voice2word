//! Zed / Codex 极简暗黑主题规范

use gpui::{rgb, Rgba};

pub struct Theme;

impl Theme {
    // 背景体系
    #[inline] pub fn bg_app() -> Rgba { rgb(0x0e0e11) }      // 视窗深邃底色
    #[inline] pub fn bg_sidebar() -> Rgba { rgb(0x131316) }  // 左侧边栏底色
    #[inline] pub fn bg_panel() -> Rgba { rgb(0x18181c) }    // 主内容面板
    #[inline] pub fn bg_card() -> Rgba { rgb(0x202024) }     // 卡片底色
    #[inline] pub fn bg_hover() -> Rgba { rgb(0x27272e) }    // 悬停底色
    #[inline] pub fn bg_input() -> Rgba { rgb(0x161619) }    // 输入区底色

    // 细微边框体系
    #[inline] pub fn border() -> Rgba { rgb(0x2c2c34) }      // 极细边界线
    #[inline] pub fn border_light() -> Rgba { rgb(0x383842) }

    // 文字层次
    #[inline] pub fn text_primary() -> Rgba { rgb(0xf3f4f6) }   // 主文字
    #[inline] pub fn text_secondary() -> Rgba { rgb(0xa1a1aa) } // 次要辅助说明
    #[inline] pub fn text_muted() -> Rgba { rgb(0x6b7280) }     // 暗提示文字
    #[inline] pub fn text_code() -> Rgba { rgb(0x38bdf8) }      // 等宽代码/时间戳蓝色

    // 状态与重音色
    #[inline] pub fn accent_primary() -> Rgba { rgb(0x6366f1) } // 主重音紫蓝
    #[inline] pub fn accent_mint() -> Rgba { rgb(0x10b981) }    // 成功/开始/进度条绿
    #[inline] pub fn accent_blue() -> Rgba { rgb(0x6366f1) }    // 标识与重音紫蓝
    #[inline] pub fn accent_red() -> Rgba { rgb(0xf43f5e) }     // 取消/错误粉红
    #[inline] pub fn accent_orange() -> Rgba { rgb(0xf59e0b) }  // 阶段高亮橙
}
