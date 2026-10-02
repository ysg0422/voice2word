# UI 设计规范

本文档描述 `src/ui/theme.rs`（token 层）与 `src/ui/primitives.rs`（原语层）构成的
设计系统。**两者是唯一事实来源**：本文只复述其中已有的内容，新增组件请回到源码
补充，而不是在本文里发明常量。

- token 层：`src/ui/theme.rs` —— 颜色角色、字号阶梯、圆角阶梯、间距阶梯、
  页面外壳、卡片、控件高度阶梯、小元素尺寸、骨架尺寸、表格尺寸、区域约束。
- 原语层：`src/ui/primitives.rs` —— 卡片、页面外壳、标题、徽标、状态点、进度条、
  按钮、分段选项、行内胶囊、圆角胶囊、模态、标题栏按钮、输入框、分隔线。

---

## 1. 为什么需要这一层

重构前，同一个视觉概念在各文件里各写一份，参数互不相同，「对齐」只能靠逐个文件
比对，改一处就漂一处。源码注释中明确记录的分歧：

- **卡片**：内边距 6 种（`4 / 12 / 16 / 20 / 24 / 40`，另有 `px_4 py_1` 手写），圆角 2 种
  （`rounded_xl` 与 `rounded_2xl` 混用）。
- **「可选项」**：4 份实现（`seg_option` / `tier_pill` / `pill_row` / `choice_pill`），
  高度 28/34/26/自动，圆角 lg/lg/md/full，选中态配色 mint/blue/primary 三种。
- **按钮与控件**：控件高度 7 种（`26/28/30/34/36/38/42`），主按钮内边距 9 种组合，
  标题栏系统按钮宽度 `40/40/44`、字形字号 `12/11/13` 不一。
- **状态点**：直径 `6/7/8` 三种。
- **页面外壳**：内边距 24（视频库/性能页）与 20（语音转写页）两种，切页时内容横向跳动；
  分区间距 `12/16/20` 三种；卡片间距 `8/10/12/14/16` 五种。
- **字号**：页面大标题 `17/18/20` 三种，区块标题 `11/12/13/14` 四种。
- **重复字面量**：样式面板标签列宽 `54` 在字号/字间距/底边距/行间距/单行字数里各写一遍
  （5 处）；时间轴轨名列宽 `70` 在 `render_multitrack_timeline`、`render_waveform_lane`、
  标尺内缩、`seek_by_mouse_x` 里各写一遍（4 处）。

token 层把这些数值收成「阶梯」，原语层把外观收成唯一实现，于是对齐变成可推导的。

---

## 2. 规则

1. **调用点只引用 token，不写裸数值。** 不再出现 `rgb(0x..)` 颜色字面量，也不在调用点
   写 `px(24)` 这类间距数字；数值一律取自 `Theme` 的阶梯常量（或等值的 tailwind 助手）。
2. **外观来自 `primitives::*`，交互由调用点接。** 原语是**外观-only**的：返回普通
   `Div`（或 `Stateful<Div>`），不持有 `Context<Self>`，因此可脱离 GPUI 上下文复用。
   `.id()` / `.on_click()` / `.track_focus()` / `.hover()`（原语之外的）由调用点接上：
   ```rust
   primitives::btn("开始转写", BtnSize::Lg, BtnVariant::Primary)
       .id("start-btn")
       .on_click(cx.listener(...))
   ```
   例外：原语内部已经接好纯外观性的悬停反馈（如 `btn_clickable` 的 `.hover()`）。
3. **尺寸只从 `Theme` 阶梯取。** 字号取 `TEXT_*`，圆角取 `RADIUS_*`，间距取 `SPACE_*`，
   控件高度取 `CTRL_H_*` / `CHIP_H`。
   **圆角默认是方角**（`RADIUS_*` 4~12px），全站控件形状统一。全圆角只允许出现在两类
   地方：媒体控制条的 iOS 胶囊（`pill_btn_*` 一族）与本来就该是圆形的元素（状态点、
   步骤圆点、圆形关闭键、头像位）。卡片里的普通按钮一律走 `btn` 的方角——
   同一个按钮列里混进一个胶囊，视觉上会非常突兀。
   **多面板容器用标头分段选项切换，不做纵向堆叠。** 面板高度有限时，两张卡片叠着放
   必须滚动才能看到下面那张；此时应改成互斥视图，标头放一对
   `segmented(..., false)`（见剪辑工作台的 `EditorSubtitlePanel`），一次只占一份版面。
4. **同一份内容不在两个视图里各放一份。** 互斥视图切换的是「面板的用途」，不是「同一批
   控件的两个副本」。放在某个视图里的编辑器 / 列表，另一个视图不该再放一个——两处同时
   可改，用户分不清哪份生效，两边也必然漂移。需要跨视图复用的部分（如字幕对照表）应
   留在互斥块**之外**共享；只归属于某个用途的（如样式参数、编辑卡）就只放那一侧。
   **编辑与预览必须同处一卡**：「实时预览」显示的应当是同一张卡里正在编辑的文本
   （见单句编辑卡的预览条，取自 `state.editing_text`），把编辑器与预览分到两个面板
   等于预览了个空。
5. **颜色按角色命名，不按色值命名**（`bg_card` 而不是 `gray_800`）。

### 2.1 何时用哪个原语

| 原语 | 何时使用 | 不要与它混淆 |
| --- | --- | --- |
| `card` | 通用内容卡片（16px 内边距） | `card_rows`（分隔线列表）、`card_sm`（侧栏紧凑） |
| `card_sm` | 信息密度高的位置，如侧栏配置块（12px 内边距） | `card` |
| `card_with_pad` | 需要非标准内边距时；内边距仍只从 `CARD_PAD*` / `SPACE_*` 取 | 直接手写 `div().p()` |
| `card_rows` | 「分隔线列表」式卡片，内部由 `setting_row` + `divider` 组成，纵向节奏交给各行 | `card`（会叠加出 26px+ 行距，过松） |
| `setting_row` | 「标签 — 控件」设置行，左标签右控件右对齐 | 自行拼 `flex` 行 |
| `page_shell` | 四个工作台页面的外壳（背景/内边距/分区间距/滚动） | 自行 `div().p()`；返回 `Stateful` |
| `page_title` | 页面大标题（18px） | `panel_title`（14px）、`section_title`（12px） |
| `panel_title` | 抽屉/检查器等面板标头（14px） | `page_title`（页面级，更高一档） |
| `section_title` | 卡片内小标题（12px 中等灰） | `field_label`（更弱一档） |
| `field_label` | 控件上方的小字组名（11px 弱化） | `section_title` |
| `badge` | 中性徽标：计数、单位等元信息（20px 高，方角） | `count_tag`（卡片内、更宽） |
| `badge_accent` | 需要吸引注意的元信息（进度百分比、已选计数） | `badge`（中性） |
| `badge_danger` | 失败计数 | `badge_accent` |
| `stat_dot` | 区块标题旁的状态点（8px） | `stat_dot_sm`（行内，6px） |
| `stat_dot_sm` | 与 11~12px 文字并排的紧凑状态点 | `stat_dot` |
| `progress_bar` | 单色进度条（0.0~1.0） | `meter_bar`（双层占用对比） |
| `meter_bar` | 硬件监控 CPU/内存：底层系统占用 + 上层进程占用 | `progress_bar`（单值） |
| `btn` | 按钮外观；禁用态等自定义行为自行拼装 | `btn_clickable`、`pill_btn_*` |
| `btn_clickable` | 次级按钮，已接 `.cursor_pointer()` 与悬停反馈 | `btn`（需自定义时用） |
| `segmented` | 可选中的分段选项（28px 高，**蓝底蓝边**选中态） | `chip`（21px 行内）、`pill_*`（非选中语义） |
| `segmented_row` | 一行分段选项容器，等宽排布 | `segmented_cluster`（自适应宽度） |
| `segmented_cluster` | 两三项、不需等分整行的分段选项行 | `segmented_row` |
| `chip` | 行内小胶囊（21px 高），嵌在卡片里的属性面板 | `segmented`（28px）、`mini_btn` |
| `chip_clickable` | 可点击的行内胶囊 | `chip`（纯外观） |
| `mini_btn` | 行内迷你次级按钮（拆分/合并），带 `enabled` 禁用态 | `chip`（选中语义） |
| `pill_btn` | 弱化胶囊，透明底悬停显形，位于已有底色的容器里 | `pill_btn_outline`（独立存在） |
| `pill_btn_outline` | 描边胶囊，独立存在的次级操作（有底色与描边） | `pill_btn` |
| `pill_btn_solid` | 实心强调胶囊（播放/暂停），`bg` 由调用点给 | `pill_btn_solid_state` |
| `btn_mint_soft` | 薄荷浅底次级按钮（「耗时详情」这类次一级操作），方角 | `btn(Primary)`（实心，主操作） |
| `btn_sm_outline` | 小号方角次级按钮（标头右侧轻量操作，21px 高/11px 字） | `btn`（更高更宽） |
| `tag_tinted` | 带色底小号状态标（「播放中」），方角，色由调用点给 | `badge_accent` |
| `count_tag` | 区块标题右侧的计数徽标（「581 句 / 已选 1/581」），方角 | `badge`（更小、中性元信息） |
| `pill_btn_outline_state` | 带禁用态的描边胶囊 | `pill_btn_outline`（无禁用态） |
| `pill_btn_solid_state` | 带禁用态的实心胶囊；禁用时退化为中性底槽，形状不变 | `pill_btn_solid` |
| `btn_danger` | 危险操作按钮（红底红边红字，悬停加深）；`Sm` = 行内小按钮，`Lg` = 卡片级按钮 | `btn(Danger)`（方形描边，语义不同） |
| `modal_scrim` | 全屏模态遮罩（铺满、居中），调用点接 `.id()` 与面板 | —— |
| `modal_card` | 模态面板外壳（`bg_raised` + 投影），宽度/内边距由调用点给 | `card`（非浮层） |
| `icon_close_btn` | 弹窗右上角圆形关闭键 | `btn`（方形） |
| `titlebar_btn` | 标题栏系统按钮（最小化/最大化/关闭），`danger` 控制关闭按钮红底 | `btn` |
| `text_input` | 单行输入框外观，`focused` 控制高亮描边；事件绑定由调用点接 | 手写输入框容器 |
| `divider` | 卡片内分组的极弱横线（1px） | `step_connector`（步骤条短横线） |
| `step_connector` | 步骤条两个步骤之间的短横线 | `divider` |

---

## 3. 颜色语义

### 3.1 双主题

每个颜色 token 都是「**深色值 / 浅色值**」二元组（`Theme::c(dark, light)` 或带 alpha 的
`Theme::a(dark, light)`），运行时按全局开关（`LIGHT_MODE` 原子量）二选一。因此调用点
**无需感知主题**：`Theme::bg_card()` 在两种主题下都成立。

### 3.2 强调底配什么字色：按实测对比度定，不按「深色主题下顺眼」

三类强调底，配三种字色，判据是**黑白两种主题下的对比度都过 AA**：

| 强调底 | 浅色值 | 深色值 | 字色 | 理由 |
| --- | --- | --- | --- | --- |
| 薄荷实心（CTA / 保存 / 播放） | `0x0d9668` | `0x10b981` | `text_on_accent`（恒黑） | 黑字 5.6:1 / 7.8:1；白字只有 3.8:1 |
| 蓝 / 红 / 深薄荷实心（选中态、终止） | `0x4f46e5` / `0xbe123c` | `0x6366f1` / `0xe11d48` | `text_on_saturated`（恒白） | 深底必须配白字，黑字仅 3.3~3.4:1 |
| 中性底槽（未选中的步骤圆点） | `0xe2e2e9` | `0x282832` | `text_primary`（翻转） | 底槽随主题翻转，字色也要跟着翻 |

一句话：**底是恒亮的 → 恒黑字；底是恒暗的 → 恒白字；底是翻转的 → 字也翻转。**
最常见的错误是「底翻转、字恒白」（浅色主题下白字压白底，看不见）。

### 3.3 薄荷绿 = 全局导航位置，蓝 = 控件选中态

这是本项目最重要的一条颜色约定，两者**不可互换、不会碰撞**：

- **薄荷绿 `accent_mint`**：表示「你当前在哪里」这类**全局导航位置**——左侧导航选中项、
  进度、成功/开始。`chip` 的选中态也用薄荷（属属性面板内的档位选择）。
- **蓝 `accent_blue`**：表示**控件内部的选择状态**——`segmented` 的选中态固定用
  「蓝底蓝边」。若这里也用绿，会与导航的「当前位置」语义混淆。

对应地，选中态的浅底/描边一律走 `tint_*` 三档（`tint_mint_soft/badge/border`、
`tint_blue_soft/badge/border`、`tint_red_*`、`tint_primary_*`、`tint_neutral`），
不再逐处手调 alpha。

### 3.4 媒体区

**监视器衬底随主题翻转**（浅色主题下是浅灰，不是纯黑）——留一整块纯黑面板会把
整个界面的明度拉塌，视觉上非常突兀。只有「压在图片上的东西」才恒深：

| token | 是否翻转 | 用途 |
| --- | --- | --- |
| `bg_media` / `bg_media_deep` | **翻转** | 监视器衬底 / 视口内屏（浅色下浅灰） |
| `text_on_media` / `accent_on_media` | **翻转** | 衬底上的占位提示文字 |
| `tint_on_media_border` | **翻转** | 衬底上的中性描边 |
| `bg_overlay` | 恒深 | 压在**图片/视频帧**上的角标条底 |
| `text_on_overlay` | 恒浅 | 角标条上的文字（与 `bg_overlay` 配对） |
| `text_subtitle` | 恒白 | 视频画面里的字幕字形 |
| `text_on_saturated` | 恒白 | 饱和色块（时间轴胶囊、蓝色选中块）上的文字 |
| `bg_scrim_soft` | 恒深半透明 | 只用来「压暗底图」，不承载文字 |

判据：**底是图片/视频帧（无法预知明度）→ 恒深底 + 恒浅字；底是自己画的衬底 →
跟着主题翻。** 视频画面里的黑色留边是视频文件自带的 pillarbox，属内容而非 UI。

> 样式预览条里没有真实画面，浅色主题下衬底是浅灰；而「电影沉浸」预设本身不带底框、
> 字形恒白。代码里由 `editor::preview_backdrop()` 在这个特殊语境下补一层深底衬，
> 只为让预览可读——监视器里压在真实画面上的字幕不受影响。

文字对比度按 WCAG AA 正文 ≥ 4.5:1 校验（`text_disabled` 只用于禁用态，不参与）。
`text_white` 名字有误导：它**会翻转**（深色主题白、浅色主题近黑），只能用在
「底色也翻转」的中性底槽上；压在恒深/恒亮底上时要用对应的恒色 token。

---

## 4. 间距阶梯

间距以 4 为基准，**所有布局间距只允许取阶梯值**。GPUI 的 tailwind 数值助手与常量
一一对应，两者取同一个值，不会出现第三种间距；用哪种形式都可以。

| 常量 | 值 (px) | 对应 tailwind 助手 |
| --- | --- | --- |
| `SPACE_0_5` | 2 | `gap_0p5` |
| `SPACE_1` | 4 | `gap_1` |
| `SPACE_1_5` | 6 | `gap_1p5` |
| `SPACE_2` | 8 | `gap_2` |
| `SPACE_2_5` | 10 | `gap_2p5` |
| `SPACE_3` | 12 | `gap_3` |
| `SPACE_4` | 16 | `gap_4` |
| `SPACE_5` | 20 | —— |
| `SPACE_6` | 24 | `gap_6` |
| `SPACE_8` | 32 | —— |

语义化派生：`PAGE_PAD = SPACE_6`、`PAGE_GAP = SPACE_5`、`CARD_PAD = SPACE_4`、
`CARD_PAD_SM = SPACE_3`、`CARD_GAP = SPACE_3`。`SPACE_1_5` 用于成组小胶囊间隙，
`SPACE_2_5` 用于成组中等控件间隙。

### 4.1 字号与圆角阶梯

**字号**（px）：`TEXT_CAPTION` 10（极小标注/徽标）→ `TEXT_SMALL` 11（次要说明）→
`TEXT_BODY` 12（正文，默认）→ `TEXT_BODY_LG` 13（强调正文/卡片标题）→ `TEXT_TITLE` 14
（区块标题）→ `TEXT_HEADING` 16（次级标题/列表主标题）→ `TEXT_STAT` 17（数字看板）→
`TEXT_DISPLAY` 20（页面大标题）。语义别名：`PAGE_TITLE = 18`、`SECTION_TITLE = 12`。

**圆角**（px）：`RADIUS_SM` 4（极小元素/时间轴片段/chip）→ `RADIUS_MD` 6（按钮、标签、
输入框）→ `RADIUS_LG` 8（常规容器）→ `RADIUS_XL` 12（卡片/面板，即 `CARD_RADIUS`）。

### 4.2 控件高度阶梯

| 常量 | 值 (px) | 用途 |
| --- | --- | --- |
| `CHIP_H` | 21 | 最密一级的行内小胶囊（`CHIP_PAD_X = 9`） |
| `CTRL_H_XS` | 24 | 行内迷你控件（编辑卡片的 ±0.1s、标点按钮） |
| `CTRL_H_SM` | 28 | 次级控件（分段选择、探测按钮、小输入框） |
| `CTRL_H_MD` | 32 | 常规控件（输入框、普通按钮） |
| `CTRL_H_LG` | 38 | 主行动按钮（开始转写 / 导出） |

`BtnSize` 映射：`Xs→CTRL_H_XS`、`Sm→CTRL_H_SM`、`Md→CTRL_H_MD`、`Lg→CTRL_H_LG`。

### 4.3 小元素尺寸

| 常量 | 值 (px) | 用途 |
| --- | --- | --- |
| `DOT_SM` / `DOT_MD` | 6 / 8 | 状态点（行内 / 区块标题旁） |
| `STEP_DOT` | 20 | 步骤条序号圆点 |
| `ICON_BOX` / `ICON_BOX_LG` | 32 / 56 | 小图标方块 / 空态拖拽导入主图形 |
| `PROGRESS_H` / `BADGE_H` | 6 / 20 | 进度条高度 / 徽标（胶囊）高度 |
| `STATUS_BAR_H` / `PROGRESS_W` | 64 / 260 | 底部状态栏高度 / 栏内进度条宽度 |
| `DIALOG_W` / `DIALOG_W_WIDE` | 520 / 580 | 弹窗面板宽度（标准 / 宽档） |
| `FORM_LABEL_W` / `CONTENT_MAX_W` | 54 / 860 | 样式面板标签列宽 / 中央内容列最大宽度 |

---

## 5. 骨架参数

布局骨架常量集中在此，调整工作台分栏、标头高度、列表可视行数时只改一处。
表末「区域约束」一组不是「控件该多大」的档位，而是**某个具体区域的尺寸约束**
（窗口缩到多小内容不塌、内容多长时开始滚动），因同属骨架参数故一并列出。

| 常量 | 值 (px) | 说明 |
| --- | --- | --- |
| `NAV_W` | 180 | 左侧主导航宽度 |
| `NAV_INDICATOR_W` | 2 | 导航选中指示条宽度 |
| `TITLEBAR_H` / `TITLEBAR_BTN_W` | 38 / 44 | 标题栏高度 / 系统按钮宽度 |
| `DRAWER_W` | 350 | 右侧配置抽屉宽度 |
| `HEADER_H` / `HEADER_H_SM` | 40 / 36 | 面板标头 / 轻量面板标头（媒体监视器）高度 |
| `BANNER_H` | 34 | 通栏提示条高度 |
| `CONTROL_BAR_H` / `CONTROL_INSET` | 52 / 3 | 媒体控制条高度 / 胶囊内衬垫 |
| `TIMELINE_H` | 134 | 编辑器底部多轨时间轴高度 |
| `TRACK_ROW_H` | 36 | 时间轴单条轨道行高（波形轨 / 字幕轨） |
| `TRACK_LABEL_W` / `TRACK_RIGHT_PAD` | 70 / 16 | 时间轴轨名标签列宽 / 右侧留白 |
| `TRACK_CLIP_INSET` | 2 | 时间轴内字幕条上下内缩 |
| `TIER_PILL_H` | 42 | 模型档位胶囊高度（两行） |
| `PLAYHEAD_W` / `PLAYHEAD_GRAB_W` | 2 / 12 | 播放头细线 / 抓取手柄宽度 |
| `RULER_TICK_W` / `RULER_TICK_H` / `RULER_TICK_HIT_W` | 1 / 6 / 32 | 时间标尺刻度线宽高 / 点击热区宽度 |
| `TABLE_HEADER_H` / `TABLE_ROW_H` | 34 / 40 | 字幕对照表表头 / 数据行高度 |
| `TABLE_STACK_H` / `TABLE_MIN_H` | 320 / 200 | 对照表堆叠布局固定高度 / 非堆叠最小高度 |
| `STYLE_PREVIEW_H` | 66 | 样式预览缩略图高度 |
| `HAIRLINE` | 1 | 发丝线（分隔线 / 刻度线 / 波形柱间隙） |
| **区域约束** | | 以下为具体区域的 min / max 约束 |
| `MIN_CENTER_COL_W` | 520 | 转写工作台中央列最小宽度 |
| `STREAM_MIN_H` | 110 | 流式字幕区保底高度（首句到达时不跳高） |
| `MONITOR_MIN_H` / `VIEWPORT_MIN_H` | 280 / 180 | 监视器 / 视口屏幕最小高度 |
| `QUEUE_LIST_MAX_H` / `QUEUE_LABEL_MAX_W` | 148 / 220 | 批量队列列表最大高度 / 行状态标签最大宽度 |
| `LIB_THUMB_W` / `LIB_THUMB_H` | 180 / 108 | 视频库列表项缩略图尺寸（16:9） |
| `LIB_ACTION_W` | 70 | 视频库列表项右侧动作列统一宽度（四按钮对齐成一列） |
| `PROBE_MSG_MAX_W` | 320 | 翻译探测结果消息最大宽度 |
| `SEG_PILL_MIN_W` | 140 | 非撑满分段胶囊最小宽度 |
| `DROPDOWN_MAX_H` | 200 | 下拉浮层最大高度 |
| `SEARCH_MIN_W` | 110 | 字幕搜索框最小宽度 |
| `SEG_CLIP_MIN_W` | 40 | 时间轴字幕条最小宽度（极短片段仍可点中） |
| `CTRL_GAP_TIGHT` | 4 | 「上句 / 下句」成组按钮紧凑间隙 |

---

## 6. 新增组件的检查清单

1. **先查已有原语。** 对照 §2.1 的表；能复用就复用，不要新写一份「看起来差不多」的。
2. **不够用才加原语。** 新原语写进 `src/ui/primitives.rs`，尺寸全部取自 `Theme` 的
   阶梯常量，不出现裸数值；返回普通 `Div`（除非确有滚动/id 需求才返回 `Stateful<Div>`）。
3. **缺档位就补阶梯。** 若确实需要新尺寸，把它加到 `Theme` 对应的阶梯（字号/圆角/间距/
   控件高度/骨架）里，**而不是在调用点内联**；并在注释里写明迁移前的分歧值。
4. **交互留在调用点。** `.id()` / `.on_click()` / `.track_focus()` 由使用处接，
   原语保持外观-only。
5. **颜色走角色 token。** 新颜色只在 `Theme` 里加，按角色命名；涉及媒体区的用
   `*_on_media` / `text_on_saturated` / `text_subtitle` 等恒定浅色 token。
6. **跑 `cargo check --lib` 验证**，确认阶梯常量与调用点都对得上。
