//! UI 原语层：把「卡片 / 区块标题 / 徽标 / 按钮 / 分段选项 / 状态点 / 进度条」
//! 这几类反复出现的视觉单元收敛成唯一实现。
//!
//! ## 为什么需要这一层
//! 迁移前同一个概念在不同文件里各写一份，参数还都不一样：
//! - 「卡片」有 6 种内边距（4/12/16/20/24/40）与 2 种圆角（xl/2xl）
//! - 「可选项」有 4 份实现（`seg_option` / `tier_pill` / `pill_row` / `choice_pill`），
//!   高度分别是 28/34/26/自动，圆角分别是 lg/lg/md/full
//! - 「状态点」直径有 6/7/8 三种
//! - 主按钮有 9 种内边距组合
//!
//! 于是「对齐」只能靠逐个文件比对，改一处就漂一处。这里把它们收敛成原语，
//! 所有尺寸取自 [`Theme`] 的阶梯常量，调用点不再出现裸数值。
//!
//! ## 用法约定
//! 原语只负责**外观**，返回普通 [`Div`]（非交互）。交互（`.id()` / `.on_click()` /
//! `.hover()` 之外的状态）由调用点接上，这样原语不必持有 `Context<Self>`，
//! 可以脱离 GPUI 上下文复用：
//! ```ignore
//! primitives::btn("开始转写", BtnSize::Lg, BtnVariant::Primary)
//!     .id("start-btn")
//!     .on_click(cx.listener(...))
//! ```

use gpui::prelude::*;
use gpui::{
    div, px, relative, AnyElement, Div, FontWeight, ParentElement, Rgba, SharedString, Stateful,
    StatefulInteractiveElement, Styled,
};

use super::theme::Theme;

// ==================== 卡片 ====================

/// 标准卡片：统一圆角、内边距、底色与描边。
pub fn card() -> Div {
    card_with_pad(Theme::CARD_PAD)
}

/// 紧凑卡片：信息密度高的位置（侧栏配置块）。
pub fn card_sm() -> Div {
    card_with_pad(Theme::CARD_PAD_SM)
}

/// 指定内边距的卡片。**内边距只应从 [`Theme`] 的 `CARD_PAD*` / `SPACE_*` 取**。
pub fn card_with_pad(pad: f32) -> Div {
    div()
        .p(px(pad))
        .rounded(px(Theme::CARD_RADIUS))
        .bg(Theme::bg_card())
        .border_1()
        .border_color(Theme::border())
        .flex()
        .flex_col()
        .gap(px(Theme::CARD_GAP))
}

/// 「分隔线列表」式卡片：内部由若干 [`setting_row`] 加 [`divider`] 组成，
/// 纵向节奏交给各行的 padding，卡片自身只留横向内边距。
///
/// 为什么单独一档：这类卡片若用标准 [`card`] 的 16px 纵向内边距，
/// 再叠加每行的 padding，行间距会到 26px 以上、列表被撑得很松；
/// 迁移前它们用 `px_4 py_1` 手写，正是为了避开这一点。这里把它固化成原语，
/// 既保留紧凑观感，又不再出现裸数值。
pub fn card_rows() -> Div {
    div()
        .w_full()
        .px(px(Theme::CARD_PAD))
        .py(px(Theme::SPACE_1))
        .rounded(px(Theme::CARD_RADIUS))
        .bg(Theme::bg_card())
        .border_1()
        .border_color(Theme::border())
        .flex()
        .flex_col()
}

/// 「标签 — 控件」设置行：左标签，右侧控件右对齐。
pub fn setting_row(label: impl Into<SharedString>, control: AnyElement) -> Div {
    div()
        .w_full()
        .flex()
        .items_center()
        .gap(px(Theme::SPACE_4))
        .py(px(Theme::SPACE_2))
        .child(
            div()
                .flex_shrink_0()
                .text_size(px(Theme::TEXT_BODY_LG))
                .font_weight(FontWeight::BOLD)
                .text_color(Theme::text_primary())
                .child(label.into()),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .flex()
                .items_center()
                .justify_end()
                .child(control),
        )
}

// ==================== 页面外壳 ====================

/// 页面外壳：统一的背景、内边距与分区间距。
/// 四个工作台页面都用它，切页时内容不会横向跳动。
///
/// 返回 [`Stateful`]，因为滚动（`overflow_y_scroll`）在 GPUI 里只对带 `id` 的元素可用。
/// 页面本来也需要一个稳定 id 供 GPUI 记录滚动位置，这里一并接好。
pub fn page_shell(id: &'static str) -> Stateful<Div> {
    div()
        .id(id)
        .w_full()
        .h_full()
        .bg(Theme::bg_panel())
        .overflow_y_scroll()
        .p(px(Theme::PAGE_PAD))
        .flex()
        .flex_col()
        .gap(px(Theme::PAGE_GAP))
}

/// 页面大标题（各页统一 18px）。
pub fn page_title(text: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(Theme::PAGE_TITLE))
        .font_weight(FontWeight::BOLD)
        .text_color(Theme::text_primary())
        .child(text.into())
}

/// 面板标题（抽屉 / 检查器等面板的标头，14px）。
/// 比 [`page_title`] 低一档：面板标头是「这一栏是什么」，
/// 页面标题是「你在哪个工作台」，两者不该同字号。
pub fn panel_title(text: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(Theme::TEXT_TITLE))
        .font_weight(FontWeight::BOLD)
        .text_color(Theme::text_primary())
        .child(text.into())
}

/// 区块标题（卡片内的小标题，统一 12px 中等灰）。
pub fn section_title(text: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(Theme::SECTION_TITLE))
        .font_weight(FontWeight::BOLD)
        .text_color(Theme::text_secondary())
        .child(text.into())
}

/// 字段标签（控件上方的小字，比 [`section_title`] 更弱一档）。
/// 用于「识别语言」「GPU 占用策略」这类成组控件的组名。
pub fn field_label(text: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(Theme::TEXT_SMALL))
        .font_weight(FontWeight::BOLD)
        .text_color(Theme::text_muted())
        .child(text.into())
}

// ==================== 徽标 / 胶囊 ====================

/// 中性徽标：计数、单位等元信息。
pub fn badge(text: impl Into<SharedString>) -> Div {
    badge_base()
        .bg(Theme::bg_inset())
        .border_color(Theme::border_mid())
        .text_color(Theme::text_secondary())
        .child(text.into())
}

/// 强调徽标：进度百分比、已选计数等需要吸引注意的元信息。
pub fn badge_accent(text: impl Into<SharedString>) -> Div {
    badge_base()
        .bg(Theme::tint_mint_soft())
        .border_color(Theme::tint_mint_border())
        .text_color(Theme::accent_mint())
        .child(text.into())
}

/// 危险徽标：失败计数。
pub fn badge_danger(text: impl Into<SharedString>) -> Div {
    badge_base()
        .bg(Theme::tint_red_soft())
        .border_color(Theme::tint_red_border())
        .text_color(Theme::accent_red())
        .child(text.into())
}

fn badge_base() -> Div {
    div()
        .h(px(Theme::BADGE_H))
        .px(px(Theme::SPACE_2))
        .rounded(px(Theme::RADIUS_SM))
        .border_1()
        .flex()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .text_size(px(Theme::TEXT_SMALL))
        .font_weight(FontWeight::MEDIUM)
}

// ==================== 状态点 ====================

/// 紧凑状态点（行内，与 11~12px 文字并排）。
pub fn stat_dot_sm(color: Rgba) -> Div {
    dot(Theme::DOT_SM, color)
}

/// 标准状态点（区块标题旁）。
pub fn stat_dot(color: Rgba) -> Div {
    dot(Theme::DOT_MD, color)
}

fn dot(size: f32, color: Rgba) -> Div {
    div()
        .w(px(size))
        .h(px(size))
        .rounded_full()
        .flex_shrink_0()
        .bg(color)
}

// ==================== 进度条 ====================

/// 单色进度条（0.0~1.0）。
pub fn progress_bar(ratio: f32) -> Div {
    let r = ratio.clamp(0.0, 1.0);
    div()
        .w_full()
        .h(px(Theme::PROGRESS_H))
        .rounded_full()
        .bg(Theme::bg_track())
        .child(
            div()
                .h_full()
                .w(relative(r))
                .rounded_full()
                .bg(Theme::accent_mint()),
        )
}

// ==================== 按钮 ====================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BtnVariant {
    /// 主行动（薄荷实心）
    Primary,
    /// 次级（抬升底 + 描边）
    Secondary,
    /// 弱化（透明底，悬停才显形）
    Ghost,
    /// 危险（红字红边）
    Danger,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BtnSize {
    /// 行内迷你（编辑卡片的 ±0.1s、标点按钮）
    Xs,
    /// 次级控件（分段选项、探测按钮）
    Sm,
    /// 常规控件
    Md,
    /// 主行动按钮
    Lg,
}

impl BtnSize {
    fn height(self) -> f32 {
        match self {
            Self::Xs => Theme::CTRL_H_XS,
            Self::Sm => Theme::CTRL_H_SM,
            Self::Md => Theme::CTRL_H_MD,
            Self::Lg => Theme::CTRL_H_LG,
        }
    }

    fn pad_x(self) -> f32 {
        match self {
            Self::Xs => Theme::SPACE_2,
            Self::Sm => Theme::SPACE_3,
            Self::Md => Theme::SPACE_4,
            Self::Lg => Theme::SPACE_5,
        }
    }

    fn radius(self) -> f32 {
        match self {
            Self::Xs | Self::Sm => Theme::RADIUS_MD,
            Self::Md => Theme::RADIUS_LG,
            Self::Lg => Theme::RADIUS_LG,
        }
    }

    fn text_size(self) -> f32 {
        match self {
            Self::Xs | Self::Sm => Theme::TEXT_SMALL,
            Self::Md => Theme::TEXT_BODY,
            Self::Lg => Theme::TEXT_BODY_LG,
        }
    }
}

/// 按钮外观。调用点自行接 `.id()` / `.on_click()` / `.hover()`。
///
/// 圆角走 [`BtnSize::radius`] 的方角阶梯（4~8px）——这是**全站控件的默认形状**。
/// 只有两类例外允许用全圆角：播放控制条这类「iOS 胶囊」控件（走 `pill_btn_*`），
/// 以及本来就该是圆形的东西（状态点、步骤圆点、圆形关闭键）。
pub fn btn(label: impl Into<SharedString>, size: BtnSize, variant: BtnVariant) -> Div {
    let base = div()
        .h(px(size.height()))
        .px(px(size.pad_x()))
        .rounded(px(size.radius()))
        .flex()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .text_size(px(size.text_size()))
        .child(label.into());

    match variant {
        BtnVariant::Primary => base
            .bg(Theme::accent_mint())
            .text_color(Theme::text_on_accent())
            .font_weight(FontWeight::BOLD),
        BtnVariant::Secondary => base
            .bg(Theme::bg_raised())
            .border_1()
            .border_color(Theme::border_mid())
            .text_color(Theme::text_secondary())
            .font_weight(FontWeight::MEDIUM),
        BtnVariant::Ghost => base
            .bg(Theme::transparent())
            .text_color(Theme::text_secondary())
            .font_weight(FontWeight::MEDIUM),
        BtnVariant::Danger => base
            .bg(Theme::tint_red_soft())
            .border_1()
            .border_color(Theme::tint_red_border())
            .text_color(Theme::accent_red())
            .font_weight(FontWeight::MEDIUM),
    }
}

/// 可点击的次级按钮：把 `.cursor_pointer()` + 悬停反馈一次接好。
/// 需要自定义行为的（例如禁用态）用 [`btn`] 自行拼装。
pub fn btn_clickable(label: impl Into<SharedString>, size: BtnSize, variant: BtnVariant) -> Div {
    let el = btn(label, size, variant).cursor_pointer();
    match variant {
        BtnVariant::Primary => el.hover(|s| s.opacity(0.9)),
        BtnVariant::Danger => el.hover(|s| {
            s.bg(Theme::accent_red_strong())
                .text_color(Theme::text_on_saturated())
        }),
        _ => el.hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::text_primary())),
    }
}

/// 带禁用态的按钮：`enabled = false` 时置灰、不接交互。
///
/// 与 [`btn_clickable`] 的分工：那个只负责「可点 + 悬停反馈」，禁用与否由调用点
/// 用 `.when()` 自行决定；本函数把「形状不变、只换配色」的禁用观感固化下来，
/// 让「点了没反应」这类死路不再出现（调用点只需 `if enabled { .on_click(..) }`）。
pub fn btn_state(
    label: impl Into<SharedString>,
    size: BtnSize,
    variant: BtnVariant,
    enabled: bool,
) -> Div {
    let base = btn(label, size, variant);
    if enabled {
        base.cursor_pointer().hover(|s| match variant {
            BtnVariant::Primary => s.opacity(0.9),
            _ => s.bg(Theme::bg_hover()).text_color(Theme::text_primary()),
        })
    } else {
        base.bg(Theme::bg_hover_strong())
            .text_color(Theme::text_muted())
            .opacity(0.6)
    }
}

// ==================== 分段选项（单选胶囊） ====================

/// 可选中的分段选项。迁移前有四份实现（高度 26/28/34/自动，圆角 md/lg/full 各异，
/// 选中态用 mint/blue/primary 三种配色），这里统一成「蓝底蓝边」一档——
/// 全局导航已用薄荷绿表示「当前位置」，控件选中态再用绿会与导航语义混淆。
///
/// `full_width` 为真时撑满父容器并等分（配置抽屉里的成组选项），
/// 为假时按内容宽度自适应（字幕检查器里的语言标签）。
pub fn segmented(label: impl Into<SharedString>, selected: bool, full_width: bool) -> Div {
    let base = div()
        .h(px(Theme::CTRL_H_SM))
        .px(px(Theme::SPACE_3))
        .rounded(px(Theme::RADIUS_MD))
        .border_1()
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .text_size(px(Theme::TEXT_SMALL))
        .child(label.into());

    let base = if full_width {
        base.flex_1().min_w(px(0.0))
    } else {
        base.flex_shrink_0()
    };

    if selected {
        base.bg(Theme::tint_blue_soft())
            .border_color(Theme::tint_blue_border())
            .text_color(Theme::accent_blue())
            .font_weight(FontWeight::SEMIBOLD)
    } else {
        base.bg(Theme::bg_raised())
            .border_color(Theme::transparent())
            .text_color(Theme::text_secondary())
            .hover(|s| {
                s.bg(Theme::tint_neutral())
                    .text_color(Theme::text_primary())
            })
    }
}

/// 一行分段选项的容器（等宽排布）。
pub fn segmented_row() -> Div {
    div().w_full().flex().gap(px(Theme::SPACE_1_5))
}

/// 自适应宽度的分段选项行（不撑满，按内容排布）。
/// 用于「本地 Qwen / 在线 API」这类只有两三项、不需要等分整行的场合。
pub fn segmented_cluster() -> Div {
    div().flex().gap(px(Theme::SPACE_1_5))
}

// ==================== 行内胶囊 ====================

/// 行内小胶囊的外观。比 [`segmented`] 更密（21px 高），用于已经嵌在卡片里的
/// 属性面板——那里再放 28px 的控件会显得松散。
///
/// 迁移前「时间微调 ±0.1s / 标点注入 / 翻译档位 / 目标语言」各写一份，
/// 高度 21~24、圆角 sm/full/md 三种、字号 11/12 混用。
///
/// `full_width` 为真时按容器等分（成组的档位/语言选项），否则按内容自适应。
pub fn chip(label: impl Into<SharedString>, selected: bool, full_width: bool) -> Div {
    let base = div()
        .h(px(Theme::CHIP_H))
        .px(px(Theme::CHIP_PAD_X))
        .rounded(px(Theme::RADIUS_SM))
        .border_1()
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(Theme::TEXT_SMALL))
        .child(label.into());

    let base = if full_width {
        base.flex_1().min_w(px(0.0))
    } else {
        base.flex_shrink_0()
    };

    if selected {
        base.bg(Theme::tint_mint_soft())
            .border_color(Theme::accent_mint())
            .text_color(Theme::accent_mint())
            .font_weight(FontWeight::SEMIBOLD)
    } else {
        base.bg(Theme::bg_inset())
            .border_color(Theme::border_mid())
            .text_color(Theme::text_secondary())
            .font_weight(FontWeight::NORMAL)
    }
}

/// 可点击的行内胶囊：接好 `.cursor_pointer()` 与悬停反馈。
pub fn chip_clickable(label: impl Into<SharedString>, selected: bool, full_width: bool) -> Div {
    chip(label, selected, full_width)
        .cursor_pointer()
        .hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::text_primary()))
}

/// 行内迷你次级按钮（属性面板的「拆分 / 合并」）。
/// `enabled = false` 时置灰且不接交互，调用点直接用返回的元素即可。
pub fn mini_btn(label: impl Into<SharedString>, enabled: bool) -> Div {
    let base = div()
        .h(px(Theme::CHIP_H))
        .px(px(Theme::CHIP_PAD_X))
        .rounded(px(Theme::RADIUS_SM))
        .border_1()
        .border_color(Theme::border_mid())
        .flex()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .text_size(px(Theme::TEXT_SMALL))
        .child(label.into());
    if enabled {
        base.bg(Theme::bg_raised())
            .text_color(Theme::text_secondary())
            .cursor_pointer()
            .hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::text_primary()))
    } else {
        base.bg(Theme::bg_raised())
            .text_color(Theme::text_disabled())
            .opacity(0.45)
    }
}

// ==================== 圆角胶囊按钮 ====================

/// 胶囊族基底：**全圆角**、28px 高。
/// 只用于媒体控制条这类「iOS 胶囊」语境；卡片里的普通按钮一律走 [`btn`] 的方角。
fn pill_base(label: impl Into<SharedString>) -> Div {
    div()
        .h(px(Theme::CTRL_H_SM))
        .px(px(Theme::SPACE_4))
        .rounded_full()
        .cursor_pointer()
        .flex()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .text_size(px(Theme::TEXT_BODY))
        .text_color(Theme::text_secondary())
        .child(label.into())
}

/// 弱化胶囊：透明底，悬停才显形。用于已经处在容器底色里的成组控件
/// （播放控制条的上/下句、±1s）。
pub fn pill_btn(label: impl Into<SharedString>) -> Div {
    pill_base(label).hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::text_primary()))
}

/// 描边胶囊：独立存在的次级操作（监视器标头的「独立窗口」、转写中的「查看进度」）。
/// 与 [`pill_btn`] 的区别是它有底色与描边，不依赖父容器提供视觉边界。
pub fn pill_btn_outline(label: impl Into<SharedString>) -> Div {
    pill_base(label)
        .bg(Theme::bg_card())
        .border_1()
        .border_color(Theme::border())
        .hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::text_primary()))
}

/// 实心强调胶囊（播放 / 暂停）。`bg` 由调用点给，因为播放态要换色。
pub fn pill_btn_solid(label: impl Into<SharedString>, bg: Rgba) -> Div {
    pill_base(label)
        .bg(bg)
        .text_color(Theme::text_on_accent())
        .font_weight(FontWeight::BOLD)
        .hover(|s| s.opacity(0.9))
}

/// 小号次级按钮：标头右侧的「独立窗口 / 查看进度」这类轻量操作。
/// 比 [`btn`] 的 `Xs` 更紧凑（21px 高、11px 字），因为它在标头里与标题同排。
/// **方角**——与全站控件一致，不再是胶囊。
pub fn btn_sm_outline(label: impl Into<SharedString>) -> Div {
    div()
        .h(px(Theme::CHIP_H))
        .px(px(Theme::SPACE_3))
        .rounded(px(Theme::RADIUS_MD))
        .bg(Theme::bg_card())
        .border_1()
        .border_color(Theme::border())
        .flex()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .cursor_pointer()
        .text_size(px(Theme::TEXT_SMALL))
        .text_color(Theme::text_secondary())
        .hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::text_primary()))
        .child(label.into())
}

/// 带色底的小号状态标（「播放中」这类瞬时状态）。方角，与全站控件一致。
/// `soft` / `border` 由调用点给，以便复用同一形状承载不同语义色。
pub fn tag_tinted(label: impl Into<SharedString>, soft: Rgba, border: Rgba, text: Rgba) -> Div {
    div()
        .h(px(Theme::CHIP_H))
        .px(px(Theme::SPACE_2))
        .rounded(px(Theme::RADIUS_SM))
        .bg(soft)
        .border_1()
        .border_color(border)
        .flex()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .text_size(px(Theme::TEXT_CAPTION))
        .font_weight(FontWeight::MEDIUM)
        .text_color(text)
        .child(label.into())
}

/// 中号计数徽标：区块标题右侧的「581 句 / 3 项 / 已选 1/581」。
/// 比 [`badge`] 更宽（内嵌在 12px 卡片里），方角，与全站控件一致。
pub fn count_tag(label: impl Into<SharedString>) -> Div {
    div()
        .h(px(Theme::CHIP_H))
        .px(px(Theme::SPACE_2))
        .rounded(px(Theme::RADIUS_SM))
        .bg(Theme::bg_inset())
        .border_1()
        .border_color(Theme::border_mid())
        .flex()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .text_size(px(Theme::TEXT_CAPTION))
        .text_color(Theme::text_secondary())
        .child(label.into())
}

/// 带禁用态的描边胶囊（底部操作栏的「播放预览」）。
/// `enabled = false` 时置灰、不接悬停反馈；交互由调用点按 `enabled` 决定是否接。
pub fn pill_btn_outline_state(label: impl Into<SharedString>, enabled: bool) -> Div {
    let base = pill_base(label).border_1().border_color(Theme::border());
    if enabled {
        base.bg(Theme::bg_card())
            .text_color(Theme::text_primary())
            .hover(|s| s.bg(Theme::bg_hover()))
    } else {
        base.bg(Theme::bg_raised())
            .text_color(Theme::text_disabled())
    }
}

/// 带禁用态的实心胶囊（底部操作栏的「开始处理 / 开始全部」）。
/// 禁用时退化为中性底槽 + 弱化文字，形状不变，避免按钮「消失」导致布局跳动。
pub fn pill_btn_solid_state(label: impl Into<SharedString>, bg: Rgba, enabled: bool) -> Div {
    let base = pill_base(label).font_weight(FontWeight::SEMIBOLD);
    if enabled {
        base.bg(bg)
            .text_color(Theme::text_on_accent())
            .hover(|s| s.opacity(0.9))
    } else {
        base.bg(Theme::bg_hover_strong())
            .text_color(Theme::text_muted())
    }
}

// ==================== 危险操作按钮 ====================
/// 危险操作按钮：红底红边红字，用于「删除」这类不可逆操作。
/// 悬停时底色加深一档（`tint_red_border`），比单纯提亮更能表达「这是个危险动作」。
///
/// 两档形状对应两种语境：
/// - `Sm`：行内小按钮（字幕属性面板的「删除」，与同排的「拆分 / 合并下句」等高）
/// - `Lg`：卡片级整行按钮（视频库列表项的动作列，与同列的实心/中性按钮同形同高）
pub fn btn_danger(label: impl Into<SharedString>, size: BtnSize) -> Div {
    let base = div()
        .border_1()
        .border_color(Theme::tint_red_border())
        .bg(Theme::tint_red_soft())
        .flex()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .cursor_pointer()
        .font_weight(FontWeight::MEDIUM)
        .text_color(Theme::accent_red())
        .hover(|s| s.bg(Theme::tint_red_border()))
        .child(label.into());

    match size {
        BtnSize::Xs | BtnSize::Sm => base
            .h(px(Theme::CHIP_H))
            .px(px(Theme::SPACE_2_5))
            .rounded(px(Theme::RADIUS_MD))
            .text_size(px(Theme::TEXT_SMALL)),
        _ => base
            .h(px(Theme::CTRL_H_MD))
            .px(px(Theme::SPACE_3))
            .rounded(px(Theme::RADIUS_LG))
            .text_size(px(Theme::TEXT_BODY)),
    }
}

/// 薄荷浅底次级按钮：薄荷色的「次一级操作」——比 [`btn`] 的 `Primary` 弱
/// （不是实心薄荷，而是薄荷浅底 + 薄荷字），比 `Secondary` 强（有色彩倾向）。
/// 卡片级方角尺寸，与同排的实心/中性按钮同高同形。
pub fn btn_mint_soft(label: impl Into<SharedString>) -> Div {
    div()
        .h(px(Theme::CTRL_H_MD))
        .px(px(Theme::SPACE_3))
        .rounded(px(Theme::RADIUS_LG))
        .bg(Theme::tint_mint_soft())
        .border_1()
        .border_color(Theme::tint_mint_border())
        .flex()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .cursor_pointer()
        .text_size(px(Theme::TEXT_BODY))
        .font_weight(FontWeight::MEDIUM)
        .text_color(Theme::accent_mint())
        .hover(|s| s.bg(Theme::tint_mint_badge()))
        .child(label.into())
}

// ==================== 双层仪表条 ====================

/// 双层占用条：底层铺系统占用（暗轨），上层叠当前进程占用（高亮）。
/// 硬件监控的 CPU / 内存对比用——两条叠加才能一眼看出「应用吃掉了系统的多少」。
pub fn meter_bar(system_ratio: f32, proc_ratio: f32, proc_color: Rgba) -> Div {
    let sys = system_ratio.clamp(0.0, 1.0);
    let proc = proc_ratio.clamp(0.0, 1.0);
    div()
        .w_full()
        .h(px(Theme::PROGRESS_H))
        .rounded(px(Theme::RADIUS_SM))
        .bg(Theme::bg_panel())
        .relative()
        .overflow_hidden()
        .child(
            div()
                .absolute()
                .top_0()
                .left_0()
                .h_full()
                .w(relative(sys))
                .bg(Theme::bg_stat_track()),
        )
        .child(
            div()
                .absolute()
                .top_0()
                .left_0()
                .h_full()
                .w(relative(proc))
                .bg(proc_color),
        )
}

// ==================== 标题栏系统按钮 ====================

/// 标题栏右侧的系统按钮（最小化 / 最大化 / 关闭）。
/// `danger = true` 时悬停变红底白字（关闭按钮），否则普通悬停。
pub fn titlebar_btn(glyph: &'static str, danger: bool) -> Div {
    let base = div()
        .w(px(Theme::TITLEBAR_BTN_W))
        .h_full()
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(Theme::TEXT_BODY))
        .text_color(Theme::text_secondary());
    if danger {
        base.hover(|s| {
            s.bg(Theme::accent_red_strong())
                .text_color(Theme::text_on_saturated())
        })
        .child(glyph)
    } else {
        base.hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::text_primary()))
            .child(glyph)
    }
}

// ==================== 单行输入框 ====================

/// 单行输入框外观。`focused` 控制高亮描边。
/// 事件绑定（`track_focus` / `on_key_down`）由调用点接。
pub fn text_input(focused: bool, min_w: f32) -> Div {
    div()
        .flex_1()
        .min_w(px(min_w))
        .h(px(Theme::CTRL_H_MD))
        .px(px(Theme::SPACE_2))
        .rounded(px(Theme::RADIUS_MD))
        .bg(Theme::bg_input())
        .border_1()
        .border_color(if focused {
            Theme::accent_blue()
        } else {
            Theme::border_mid()
        })
        .cursor_text()
        .flex()
        .items_center()
        .overflow_hidden()
        .text_size(px(Theme::TEXT_BODY))
}

// ==================== 模态弹窗 ====================

/// 全屏模态遮罩：铺满窗口、居中承载面板。
/// 返回普通 [`Div`]，调用点接 `.id()` 与面板子元素。
pub fn modal_scrim() -> Div {
    div()
        .absolute()
        .inset_0()
        .bg(Theme::bg_scrim())
        .flex()
        .items_center()
        .justify_center()
}

/// 模态面板外壳：与全站卡片同源（圆角 / 描边 / 纵向排布），
/// 只是底色再抬一档（`bg_raised`）并加投影浮起。
///
/// 迁移前 benchmark / completion 两个弹窗各手写一遍这段外壳，
/// 宽度、内边距、投影参数还略有出入。
pub fn modal_card(width: f32, pad: f32) -> Div {
    card_with_pad(pad)
        .w(px(width))
        .bg(Theme::bg_raised())
        .border_color(Theme::border_strong())
        .shadow_lg()
}

/// 圆形图标关闭键（弹窗右上角）：把次级按钮压成等宽圆形。
pub fn icon_close_btn() -> Div {
    btn("✕", BtnSize::Sm, BtnVariant::Ghost)
        .w(px(Theme::CTRL_H_SM))
        .h(px(Theme::CTRL_H_SM))
        .px(px(0.0))
        .rounded_full()
        .bg(Theme::bg_hover_strong())
        .text_color(Theme::text_secondary())
        .cursor_pointer()
        .hover(|s| {
            s.bg(Theme::border_strong())
                .text_color(Theme::text_primary())
        })
}

/// 内嵌清单的滚轮「吃掉 / 放行」判据（纯函数，便于单测）。
///
/// # 为什么需要它
///
/// GPUI 的内建滚动监听（`paint_scroll_listener`）与自定义 `on_scroll_wheel` 都
/// **不会** `stop_propagation`。因此鼠标悬停在内嵌可滚动清单上滚动时，事件会继续
/// 冒泡到外层可滚动面板，清单与外层面板一起滚——用户看到的就是
/// 「在字幕清单里滚轮，整个界面也跟着滚」。
///
/// 这里给出判据：清单在滚轮方向上**还有余量**就吃掉这次事件；已经顶到上/下边界
/// 就放行，交给外层接管（滚动链接，符合用户对嵌套滚动的预期）。
///
/// - `delta_y`：本次滚轮的纵向增量（正 = 向上滚，负 = 向下滚）。
/// - `offset_y`：清单当前纵向偏移（≤ 0，越负表示越靠下）。
/// - `max_h`：清单可滚动的最大高度（≥ 0；0 表示内容没超框、根本不可滚）。
///
/// 返回 `true` = 清单自己吃掉（阻止冒泡）；`false` = 放行给外层。
pub fn should_consume_scroll(delta_y: f32, offset_y: f32, max_h: f32) -> bool {
    // 横向滚轮（dy == 0）不拦；清单不可滚（max_h≈0）一律放行，让外层接管。
    if delta_y == 0.0 || max_h <= 0.5 {
        return false;
    }
    // 半个像素的容差：浮点布局算出的边界常带 1e-5 级误差，直接比 0 会误判。
    if delta_y > 0.0 {
        offset_y < -0.5
    } else {
        offset_y > -(max_h - 0.5)
    }
}

// ==================== 分隔线 ====================

/// 卡片内分组用的极弱横线。
pub fn divider() -> Div {
    div()
        .w_full()
        .h(px(Theme::HAIRLINE))
        .bg(Theme::border_subtle())
}

/// 步骤条里两个步骤之间的短横线。
pub fn step_connector() -> Div {
    div()
        .w(px(Theme::SPACE_6))
        .h(px(Theme::HAIRLINE))
        .bg(Theme::bg_hover_strong())
        .flex_shrink_0()
}
