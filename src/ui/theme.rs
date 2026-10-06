//! 设计系统：颜色 / 字号 / 圆角 token。
//!
//! 约定：UI 层只引用 token，不再直接写 `rgb(0x..)` 字面量。
//! 颜色按「角色」命名（surface / border / text / tint），不按色值命名。
//!
//! ## 双主题
//! 每个 token 都是一个「深色值 / 浅色值」二元组，运行时按全局开关二选一，
//! 因此调用点无需感知主题（`Theme::bg_card()` 在两种主题下都成立）。
//! 例外是「媒体区」相关 token（`bg_media` / `text_on_media` / `text_subtitle` /
//! `text_on_saturated`）：视频画面与时间轴胶囊在任何主题下都是深底，
//! 这些位置必须恒定用浅色文字，否则浅色主题下会变成深底深字。
//!
//! 文字对比度（深色主题按最亮的常规底色 `bg_hover`、浅色主题按 `bg_hover` 校验，
//! WCAG AA 正文要求 ≥ 4.5:1）：
//! - 深色：`text_primary` 13.5~17.5、`text_secondary` 5.8~7.5、`text_muted` 4.9~6.3
//! - 浅色：`text_primary` 15.8~17.2、`text_secondary` 6.2~7.1、`text_muted` 4.6~5.2
//! `text_disabled` 只用于禁用态，不参与正文对比度要求。

use gpui::{rgb, rgba, Rgba};
use std::sync::atomic::{AtomicBool, Ordering};

/// 全局主题开关。放在原子量里，避免把主题状态穿进每一个渲染函数签名。
static LIGHT_MODE: AtomicBool = AtomicBool::new(false);

pub struct Theme;

impl Theme {
    /// 切换主题（由设置页 / 侧边栏开关调用，并在启动时按配置初始化）
    pub fn set_light(on: bool) {
        LIGHT_MODE.store(on, Ordering::Relaxed);
    }

    /// 当前是否为浅色主题
    pub fn is_light() -> bool {
        LIGHT_MODE.load(Ordering::Relaxed)
    }

    /// 深色值 / 浅色值二选一（不透明色）
    #[inline]
    fn c(dark: u32, light: u32) -> Rgba {
        rgb(if Self::is_light() { light } else { dark })
    }

    /// 深色值 / 浅色值二选一（带 alpha，形如 0xRRGGBBAA）
    #[inline]
    fn a(dark: u32, light: u32) -> Rgba {
        rgba(if Self::is_light() { light } else { dark })
    }

    // ==================== 背景层次（由深到浅） ====================
    /// 窗口最底色
    #[inline] pub fn bg_app() -> Rgba { Self::c(0x0e0e11, 0xf2f2f5) }
    /// 视频监视器 / 样式预览条底色。
    ///
    /// **随主题翻转**：深色主题下是接近纯黑（视频画面的常规衬底），浅色主题下是
    /// 一层浅灰——浅色主题里留一整块纯黑面板会非常突兀，也会把整个界面的明度
    /// 拉塌。真正「必须恒深」的只有压在图片上的角标条（见 [`Self::bg_overlay`]）。
    #[inline] pub fn bg_media() -> Rgba { Self::c(0x09090b, 0xe8e8ee) }
    /// 监视器视口内屏（比 bg_media 再深一档，用于区分「画面区」与「画面外的衬底」）
    #[inline] pub fn bg_media_deep() -> Rgba { Self::c(0x050507, 0xdedee6) }
    /// 比窗口底更深的容器
    #[inline] pub fn bg_deep() -> Rgba { Self::c(0x0a0a0f, 0xececf0) }
    /// 左侧导航 / 标题栏 / 状态栏 / 时间轴
    #[inline] pub fn bg_sidebar() -> Rgba { Self::c(0x131316, 0xfafafc) }
    /// 输入框 / 代码块 / 内嵌凹槽
    #[inline] pub fn bg_input() -> Rgba { Self::c(0x161619, 0xffffff) }
    /// 主内容面板
    #[inline] pub fn bg_panel() -> Rgba { Self::c(0x18181c, 0xf7f7fa) }
    /// 面板内的次级内嵌区块
    #[inline] pub fn bg_inset() -> Rgba { Self::c(0x181820, 0xeeeef2) }
    /// 抬升一级的区块（比 bg_inset 略亮）
    #[inline] pub fn bg_raised() -> Rgba { Self::c(0x1a1a24, 0xf1f1f5) }
    /// 禁用态底色
    #[inline] pub fn bg_disabled() -> Rgba { Self::c(0x1e1e26, 0xe8e8ee) }
    /// 卡片底色
    #[inline] pub fn bg_card() -> Rgba { Self::c(0x202024, 0xffffff) }
    /// 进度条轨道 / 分组底槽
    #[inline] pub fn bg_track() -> Rgba { Self::c(0x22222a, 0xe4e4ea) }
    /// 卡片悬停底色
    #[inline] pub fn bg_card_hover() -> Rgba { Self::c(0x242428, 0xf4f4f8) }
    /// 通用悬停底色
    #[inline] pub fn bg_hover() -> Rgba { Self::c(0x27272e, 0xeaeaef) }
    /// 强悬停 / 选中态底色
    #[inline] pub fn bg_hover_strong() -> Rgba { Self::c(0x282832, 0xe2e2e9) }
    /// 弹窗遮罩
    #[inline] pub fn bg_scrim() -> Rgba { Self::a(0x000000cc, 0x1a1a2066) }
    /// 卡片内浮层的浅遮罩（如缩略图底部信息条，恒为深色半透明）
    #[inline] pub fn bg_scrim_soft() -> Rgba { rgba(0x0a0a0fb8) }
    /// 恒为深色的浮层底（缩略图上的角标条）。
    ///
    /// 与 [`Self::bg_scrim_soft`] 同名同色但语义不同：`bg_scrim_soft` 是**遮罩**
    /// （压暗底下的图，可以很淡），本 token 是**承载文字的底**（必须足够深，
    /// 否则上面的浅色文字在浅色主题下会糊掉）。用它时文字取 `text_on_media`。
    #[inline] pub fn bg_overlay() -> Rgba { rgba(0x0a0a0fcc) }
    /// 资源监控里的「系统占用」衬底条
    #[inline] pub fn bg_stat_track() -> Rgba { Self::c(0x4a4a58, 0xc8c8d2) }
    /// 空态状态点
    #[inline] pub fn bg_dot_idle() -> Rgba { Self::c(0x3a3a44, 0xc4c4ce) }

    // ==================== 边框 ====================
    /// 极弱分隔（卡片内分组）
    #[inline] pub fn border_subtle() -> Rgba { Self::c(0x242430, 0xececf2) }
    /// 常规分隔线
    #[inline] pub fn border() -> Rgba { Self::c(0x2c2c34, 0xdedee6) }
    /// 中间档边界（内嵌区块、输入框）
    #[inline] pub fn border_mid() -> Rgba { Self::c(0x282832, 0xe4e4ea) }
    /// 强调边界 / 悬停边框
    #[inline] pub fn border_strong() -> Rgba { Self::c(0x383842, 0xc6c6d2) }
    /// 兼容旧名，等同于 border_strong
    #[inline] pub fn border_light() -> Rgba { Self::c(0x383842, 0xc6c6d2) }

    // ==================== 文字 ====================
    /// 主文字
    #[inline] pub fn text_primary() -> Rgba { Self::c(0xf3f4f6, 0x18181b) }
    /// 高对比文字（选中项、深色胶囊上的文字）
    #[inline] pub fn text_white() -> Rgba { Self::c(0xffffff, 0x18181b) }
    /// 次要说明
    #[inline] pub fn text_secondary() -> Rgba { Self::c(0xa1a1aa, 0x5c5c66) }
    /// 弱化标签 / 元信息（仍满足 AA）
    #[inline] pub fn text_muted() -> Rgba { Self::c(0x94949e, 0x6b6b76) }
    /// 禁用态文字（不参与正文对比度要求）
    #[inline] pub fn text_disabled() -> Rgba { Self::c(0x6b7280, 0xa1a1aa) }
    /// 等宽代码 / 时间戳
    #[inline] pub fn text_code() -> Rgba { Self::c(0x38bdf8, 0x0284c7) }
    /// **亮色**强调底（薄荷 / 橙色）上的文字，两种主题下都取深色。
    ///
    /// 判定依据是实测对比度，不是「深色主题下看着顺眼」：
    /// - 薄荷浅色值 `0x0d9668`：黑字 5.6:1（AA 通过），白字 3.8:1（不通过）
    /// - 薄荷深色值 `0x10b981`：黑字 7.8:1
    /// 所以薄荷底在黑/白两种主题下都该配黑字。
    ///
    /// 反例：蓝色 `0x4f46e5`、红色 `0xbe123c` 这类**深色**强调底必须配恒白文字，
    /// 见 [`Self::text_on_saturated`]；拿本 token 去配它们会变成深字压深底。
    #[inline] pub fn text_on_accent() -> Rgba { rgb(0x09090b) }
    /// 视频画面 / 纯黑底上的字幕字形颜色，恒为白色（不随主题变化）。
    /// 与 [`Self::text_on_media`] 的分工：本 token 给「视频里的字幕」，后者给「界面文字」。
    #[inline] pub fn text_subtitle() -> Rgba { rgb(0xffffff) }
    /// 监视器衬底上的界面文字（「正在提取视频帧…」这类占位提示）。
    /// 随主题翻转：深色主题浅灰字，浅色主题深灰字。
    #[inline] pub fn text_on_media() -> Rgba { Self::c(0xd4d4d8, 0x5c5c66) }
    /// 监视器衬底上的强调文字（「正在同步视频画面…」）。
    #[inline] pub fn accent_on_media() -> Rgba { Self::c(0x34d399, 0x0d9668) }
    /// 压在**图片**上的角标条文字（缩略图的 MP4 / 时长角标）。
    /// 恒为浅色——角标条底色 `bg_overlay` 恒深，不随主题变。
    #[inline] pub fn text_on_overlay() -> Rgba { rgb(0xd4d4d8) }
    /// 饱和色块（时间轴胶囊）上的文字，恒为白色
    #[inline] pub fn text_on_saturated() -> Rgba { rgb(0xffffff) }

    // ==================== 强调色 ====================
    #[inline] pub fn accent_primary() -> Rgba { Self::c(0x6366f1, 0x4f46e5) }
    /// 标识与重音（与 accent_primary 同色，语义区分）
    #[inline] pub fn accent_blue() -> Rgba { Self::c(0x6366f1, 0x4f46e5) }
    /// 成功 / 开始 / 进度条
    #[inline] pub fn accent_mint() -> Rgba { Self::c(0x10b981, 0x0d9668) }
    /// 薄荷深档（时间轴激活块等需要压暗的场合）
    #[inline] pub fn accent_mint_deep() -> Rgba { Self::c(0x059669, 0x047857) }
    /// 取消 / 错误
    #[inline] pub fn accent_red() -> Rgba { Self::c(0xf43f5e, 0xe11d48) }
    /// 终止按钮等需要更强的红
    #[inline] pub fn accent_red_strong() -> Rgba { Self::c(0xe11d48, 0xbe123c) }
    /// 阶段高亮 / 播放态
    #[inline] pub fn accent_orange() -> Rgba { Self::c(0xf59e0b, 0xd97706) }

    // ==================== 强调色浅底 / 描边（每色三档，不再逐处调 alpha） ====================
    /// 薄荷 · 浅底（徽标、选中行）
    #[inline] pub fn tint_mint_soft() -> Rgba { Self::a(0x10b9811a, 0x10b98126) }
    /// 薄荷 · 徽标底
    #[inline] pub fn tint_mint_badge() -> Rgba { Self::a(0x10b98126, 0x10b98133) }
    /// 薄荷 · 描边
    #[inline] pub fn tint_mint_border() -> Rgba { Self::a(0x10b98144, 0x10b98166) }
    /// 蓝 · 浅底
    #[inline] pub fn tint_blue_soft() -> Rgba { Self::a(0x38bdf81a, 0x38bdf826) }
    /// 蓝 · 徽标底
    #[inline] pub fn tint_blue_badge() -> Rgba { Self::a(0x38bdf826, 0x38bdf833) }
    /// 蓝 · 描边
    #[inline] pub fn tint_blue_border() -> Rgba { Self::a(0x38bdf866, 0x38bdf899) }
    /// 琥珀 · 浅底（提示 / 复核标记，如术语表疑似未命中）
    #[inline] pub fn tint_warn_soft() -> Rgba { Self::a(0xf59e0b1f, 0xd9770626) }
    /// 琥珀 · 描边
    #[inline] pub fn tint_warn_border() -> Rgba { Self::a(0xf59e0b55, 0xd9770677) }
    /// 红 · 浅底
    #[inline] pub fn tint_red_soft() -> Rgba { Self::a(0xf43f5e14, 0xf43f5e1f) }
    /// 红 · 描边
    #[inline] pub fn tint_red_border() -> Rgba { Self::a(0xf43f5e38, 0xf43f5e59) }
    /// 主重音 · 浅底
    #[inline] pub fn tint_primary_soft() -> Rgba { Self::a(0x6366f11a, 0x6366f126) }
    /// 主重音 · 徽标底
    #[inline] pub fn tint_primary_badge() -> Rgba { Self::a(0x6366f126, 0x6366f133) }
    /// 主重音 · 描边
    #[inline] pub fn tint_primary_border() -> Rgba { Self::a(0x6366f166, 0x6366f199) }
    /// 中性 · 浅底（悬停、选中叠加）
    #[inline] pub fn tint_neutral() -> Rgba { Self::a(0xffffff0d, 0x0000000d) }
    /// 中性 · 描边（字幕框、浮层边）
    #[inline] pub fn tint_neutral_border() -> Rgba { Self::a(0xffffff28, 0x00000024) }
    /// 监视器衬底上的中性描边（随主题翻转）
    #[inline] pub fn tint_on_media_border() -> Rgba { Self::a(0xffffff28, 0x00000024) }
    /// 完全透明（GPUI 里表示「无填充」）
    #[inline] pub fn transparent() -> Rgba { rgba(0x00000000) }

    // ==================== 字号阶梯（px） ====================
    /// 极小标注 / 徽标
    pub const TEXT_CAPTION: f32 = 10.0;
    /// 次要说明
    pub const TEXT_SMALL: f32 = 11.0;
    /// 正文（默认）
    pub const TEXT_BODY: f32 = 12.0;
    /// 强调正文 / 卡片标题
    pub const TEXT_BODY_LG: f32 = 13.0;
    /// 区块标题
    pub const TEXT_TITLE: f32 = 14.0;
    /// 次级标题 / 列表主标题（卡片里的文件名、空态标题）
    pub const TEXT_HEADING: f32 = 16.0;
    /// 数字看板
    pub const TEXT_STAT: f32 = 17.0;
    /// 页面大标题
    pub const TEXT_DISPLAY: f32 = 20.0;

    // ==================== 圆角阶梯（px） ====================
    /// 极小元素（时间轴片段）
    pub const RADIUS_SM: f32 = 4.0;
    /// 小控件（按钮、标签）
    pub const RADIUS_MD: f32 = 6.0;
    /// 常规容器
    pub const RADIUS_LG: f32 = 8.0;
    /// 卡片 / 面板
    pub const RADIUS_XL: f32 = 12.0;

    // ==================== 间距阶梯（px，4 基准） ====================
    //
    // 迁移前各文件的 padding/gap 是「随手挑一个数」：同一层级的卡片内边距出现
    // 过 4/12/16/20/24/40 六种值，页面外壳出现 20 与 24 两种，卡片间距出现
    // 8/10/12/14/16 五种。这里定一条 4 的倍数阶梯，所有布局间距只允许取阶梯值，
    // 于是「对齐」变成可推导的，而不是逐个文件比对。
    //
    // GPUI 的 tailwind 数值助手（`gap_2` = 8px、`px_4` = 16px…）与这条阶梯一一对应：
    // `gap_0p5`↔`SPACE_0_5`、`gap_1`↔`SPACE_1`、`gap_1p5`↔`SPACE_1_5`、
    // `gap_2`↔`SPACE_2`、`gap_2p5`↔`SPACE_2_5`、`gap_3`↔`SPACE_3`、`gap_4`↔`SPACE_4`、
    // `gap_6`↔`SPACE_6`。用助手或常量都可以，两者取同一个值，不会出现第三种间距。
    pub const SPACE_0_5: f32 = 2.0;
    pub const SPACE_1: f32 = 4.0;
    /// 半步（6px）：成组小胶囊之间的间隙，比 SPACE_1 松、比 SPACE_2 紧。
    pub const SPACE_1_5: f32 = 6.0;
    pub const SPACE_2: f32 = 8.0;
    /// 半步（10px）：成组中等控件之间的间隙。
    pub const SPACE_2_5: f32 = 10.0;
    pub const SPACE_3: f32 = 12.0;
    pub const SPACE_4: f32 = 16.0;
    pub const SPACE_5: f32 = 20.0;
    pub const SPACE_6: f32 = 24.0;
    pub const SPACE_8: f32 = 32.0;

    // ==================== 页面外壳 ====================
    /// 四个工作台页面的统一内边距。
    /// 迁移前：视频库/性能页 `p_6`(24)，语音转写页 `p_5`(20)，切换页面时内容会横向跳动。
    pub const PAGE_PAD: f32 = Self::SPACE_6;
    /// 页面内各分区的统一间距（迁移前 12/16/20 三种）。
    pub const PAGE_GAP: f32 = Self::SPACE_5;
    /// 页面大标题字号（迁移前 17/18/20 三种）。
    pub const PAGE_TITLE: f32 = 18.0;

    // ==================== 卡片 ====================
    /// 卡片内边距（标准）。
    /// 迁移前：`p_3`(12) / `p_4`(16) / `p_5`(20) / `p_6`(24) / `p_10`(40) / `px_4 py_1` 混用。
    pub const CARD_PAD: f32 = Self::SPACE_4;
    /// 卡片内边距（紧凑，用于侧栏等信息密度高的地方）。
    pub const CARD_PAD_SM: f32 = Self::SPACE_3;
    /// 卡片内子块之间的间距。
    pub const CARD_GAP: f32 = Self::SPACE_3;
    /// 卡片统一圆角（迁移前 `rounded_xl` 与 `rounded_2xl` 混用）。
    pub const CARD_RADIUS: f32 = Self::RADIUS_XL;

    // ==================== 控件高度阶梯 ====================
    //
    // 迁移前同一类控件出现过 26/28/30/34/36/38/42 七种高度。收成四档：
    // 行内迷你（徽标旁的小按钮）→ 次级控件 → 常规控件 → 主行动按钮。
    /// 行内迷你控件（编辑卡片里的 ±0.1s、标点按钮）
    pub const CTRL_H_XS: f32 = 24.0;
    /// 最密一级的行内小胶囊（属性面板的 ±0.1s、标点注入、档位选择）。
    /// 比 [`Self::CTRL_H_XS`] 还矮，因为它在已经 12px 内边距的面板里再嵌一层。
    pub const CHIP_H: f32 = 21.0;
    pub const CHIP_PAD_X: f32 = 9.0;
    /// 次级控件（分段选择、探测按钮、小输入框）
    pub const CTRL_H_SM: f32 = 28.0;
    /// 常规控件（输入框、普通按钮）
    pub const CTRL_H_MD: f32 = 32.0;
    /// 主行动按钮（开始转写 / 导出 / 选择文件并转写）
    pub const CTRL_H_LG: f32 = 38.0;

    // ==================== 小元素尺寸 ====================
    /// 状态点（紧凑：行内徽标旁）
    pub const DOT_SM: f32 = 6.0;
    /// 状态点（标准：区块标题旁）
    pub const DOT_MD: f32 = 8.0;
    /// 步骤条序号圆点直径（转写工作台的「1/2/3」）
    pub const STEP_DOT: f32 = 20.0;
    /// 小图标方块边长（导入卡片的「+」、列表项图标位）
    pub const ICON_BOX: f32 = 32.0;
    /// 大图标方块边长（空态拖拽导入区的主图形）
    pub const ICON_BOX_LG: f32 = 56.0;
    /// 弹窗面板宽度（标准）
    pub const DIALOG_W: f32 = 520.0;
    /// 弹窗面板宽度（宽档：完成汇总这类信息量更大的弹窗）
    pub const DIALOG_W_WIDE: f32 = 580.0;
    /// 进度条高度（迁移前 4 与 6 两种，统一到 6）。
    pub const PROGRESS_H: f32 = 6.0;
    /// 徽标高度（胶囊）
    pub const BADGE_H: f32 = 20.0;
    /// 底部状态栏高度
    pub const STATUS_BAR_H: f32 = 64.0;
    /// 底部状态栏内进度条宽度
    pub const PROGRESS_W: f32 = 260.0;
    /// 样式面板里「标签 + 滑条」行的标签列宽。
    /// 迁移前 editor.rs 里 54 这个字面量在字号/字间距/底边距/行间距/单行字数
    /// 五行里各写一遍，改一处就对不齐。
    pub const FORM_LABEL_W: f32 = 54.0;
    /// 内容列最大宽度（转写工作台的中央内容区）。
    pub const CONTENT_MAX_W: f32 = 860.0;
    /// 区块标题字号（迁移前 11/12/13/14 四种）。
    pub const SECTION_TITLE: f32 = 12.0;

    // ==================== 骨架尺寸 ====================
    /// 左侧主导航宽度
    pub const NAV_W: f32 = 180.0;
    /// 标题栏高度
    pub const TITLEBAR_H: f32 = 38.0;
    /// 标题栏系统按钮宽度（最小化 / 最大化 / 关闭）。
    /// 迁移前三者分别是 40/40/44、字形字号 12/11/13，点起来宽度会跳。
    pub const TITLEBAR_BTN_W: f32 = 44.0;
    /// 右侧配置抽屉宽度
    pub const DRAWER_W: f32 = 350.0;
    /// 编辑器底部多轨时间轴高度
    pub const TIMELINE_H: f32 = 134.0;
    /// 面板标头高度（检查器、抽屉等带标题的容器）
    pub const HEADER_H: f32 = 40.0;
    /// 轻量面板标头（媒体监视器：画面本身是视觉主体，标头不该抢高度）
    pub const HEADER_H_SM: f32 = 36.0;
    /// 通栏提示条（转写中横幅等）
    pub const BANNER_H: f32 = 34.0;
    /// 媒体控制条高度
    pub const CONTROL_BAR_H: f32 = 52.0;
    /// 时间轴左侧轨名标签列宽度。
    /// 迁移前这个 70 在 `render_multitrack_timeline`、`render_waveform_lane`、标尺
    /// 内缩、以及 `seek_by_mouse_x` 里各写了一遍字面量，改一处就错位。
    pub const TRACK_LABEL_W: f32 = 70.0;
    /// 时间轴右侧留白
    pub const TRACK_RIGHT_PAD: f32 = 16.0;
    /// 模型档位胶囊高度（两行：档位名 + 速度说明）
    pub const TIER_PILL_H: f32 = 42.0;
    /// 时间轴单条轨道的行高（波形轨 / 字幕轨）
    pub const TRACK_ROW_H: f32 = 36.0;
    /// 时间轴内字幕条的上下内缩（条与轨之间留缝，视觉上区分「轨」与「片段」）
    pub const TRACK_CLIP_INSET: f32 = 2.0;
    /// 播放头抓取手柄宽度（宽于 2px 细线，便于鼠标点中）
    pub const PLAYHEAD_GRAB_W: f32 = 12.0;
    /// 时间标尺刻度的点击热区宽度
    pub const RULER_TICK_HIT_W: f32 = 32.0;
    /// 播放头细线宽度
    pub const PLAYHEAD_W: f32 = 2.0;
    /// 媒体控制条内衬垫（圆角胶囊里再嵌一层按钮时的 3px 内缩）
    pub const CONTROL_INSET: f32 = 3.0;

    // ==================== 表格尺寸 ====================
    /// 字幕对照表表头高度
    pub const TABLE_HEADER_H: f32 = 34.0;
    /// 字幕对照表数据行高度
    pub const TABLE_ROW_H: f32 = 40.0;
    /// 样式预览缩略图高度
    pub const STYLE_PREVIEW_H: f32 = 66.0;
    /// 字幕对照表在上下堆叠布局下的固定高度。
    /// 堆叠时不能再吃 `flex_1`（自动高度的滚动容器里会塌成 0 高），须给确定高度。
    pub const TABLE_STACK_H: f32 = 320.0;

    // ==================== 区域最小 / 最大约束 ====================
    //
    // 这些不是「控件该多大」的设计档位，而是某个具体区域的尺寸约束：
    // 约束的是「窗口缩到多小内容不塌」「内容多长时开始滚动」。
    // 仍集中在此，是因为它们同属「布局骨架参数」——调整工作台分栏比例、
    // 列表可视行数时只改这里一处，不必回各 view 里翻裸数值。
    /// 转写工作台中央列的最小宽度（再窄右侧配置栏就无处安放）
    pub const MIN_CENTER_COL_W: f32 = 520.0;
    /// 流式字幕区的保底高度（空态与有内容时等高，首句到达时面板不跳高）
    pub const STREAM_MIN_H: f32 = 110.0;
    /// 批量队列列表的最大高度（超出则内部滚动，不撑开整个页面）
    pub const QUEUE_LIST_MAX_H: f32 = 148.0;
    /// 批量队列行右侧状态标签的最大宽度
    pub const QUEUE_LABEL_MAX_W: f32 = 220.0;
    /// 视频库列表项缩略图尺寸（16:9 略作裁剪）
    pub const LIB_THUMB_W: f32 = 180.0;
    pub const LIB_THUMB_H: f32 = 108.0;
    /// 视频库列表项右侧动作列的统一宽度（四个按钮同宽对齐成一列）
    pub const LIB_ACTION_W: f32 = 70.0;
    /// 翻译探测结果消息的最大宽度（避免长错误串把控制行撑开）
    pub const PROBE_MSG_MAX_W: f32 = 320.0;
    /// 非撑满的分段胶囊的最小宽度（成组排布时不被文字压塌）
    pub const SEG_PILL_MIN_W: f32 = 140.0;
    /// 左侧导航选中指示条宽度
    pub const NAV_INDICATOR_W: f32 = 2.0;
    /// 编辑器视频监视器的最小高度
    pub const MONITOR_MIN_H: f32 = 280.0;
    /// 监视器视口屏幕的最小高度
    pub const VIEWPORT_MIN_H: f32 = 180.0;
    /// 下拉浮层（导出格式选择等）的最大高度
    pub const DROPDOWN_MAX_H: f32 = 200.0;
    /// 字幕对照表卡片在非堆叠布局下的最小高度
    pub const TABLE_MIN_H: f32 = 200.0;
    /// 字幕搜索框最小宽度
    pub const SEARCH_MIN_W: f32 = 110.0;
    /// 时间轴字幕条的最小宽度（极短片段仍要能点中）
    pub const SEG_CLIP_MIN_W: f32 = 40.0;
    /// 时间标尺刻度线
    pub const RULER_TICK_W: f32 = Self::HAIRLINE;
    pub const RULER_TICK_H: f32 = 6.0;
    /// 发丝线宽度（分隔线 / 刻度线等 1px 元素）
    pub const HAIRLINE: f32 = 1.0;
    /// 波形柱之间的间隙
    pub const WAVE_BAR_GAP: f32 = Self::HAIRLINE;
    /// 「上句 / 下句」这类成组控制按钮之间的紧凑间隙
    pub const CTRL_GAP_TIGHT: f32 = Self::SPACE_1;
    /// 波形柱的最小可见高度（静音段也留一条细线，保持整轨连续）
    pub const WAVE_BAR_MIN_H: f32 = 1.0;
    /// 字幕预览条两侧「拖拽调宽」把手的宽度。
    /// 把手贴在字幕框左右两侧，拖动时按位移的**两倍**收放（两侧对称），
    /// 所以看到的是字幕框以中线为中心变宽 / 变窄。
    pub const RESIZE_HANDLE_W: f32 = 6.0;
    /// 字幕预览框的宽度约束（手动拖动的可调区间）
    pub const PREVIEW_BOX_MIN_W: f32 = 140.0;
    pub const PREVIEW_BOX_MAX_W: f32 = 560.0;
}
