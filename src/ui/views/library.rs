//! 媒体库与历史任务视图

use gpui::prelude::*;
use gpui::*;
use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;

use super::super::primitives;
use super::super::theme::Theme;
use super::super::types::{ConfirmAction, ConfirmDialogInfo};
use super::super::MainWindow;
use crate::app::state::{ProcessStatus, WorkspaceTab};
use crate::subtitle::writer::{canonical_write_format, export_spec_for};
use crate::subtitle::SubtitleWriter;
use crate::ui::apply_line_edit;
use crate::utils::time::format_duration_short;

impl MainWindow {
    /// 渲染历史视频库 (视频资产管理与一键载入工作台)
    pub(crate) fn render_library_layout(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let total_count = self.state.recent_tasks.len();

        // 缩略图分两步派发（都幂等，完成后 `cx.notify()` 自动续上下一批）：
        // 1) 解码：只补「视口内 + 上下边距」的卡片，解码结果按
        //    [`LIBRARY_THUMB_IMAGE_CAP`] 封顶（不再整库常驻内存）；
        // 2) 提取：把还没有帧文件的卡片抽一帧落盘（视口内的优先）。
        let thumb_demand = self.library_thumb_decode_demand(cx);
        self.ensure_library_thumb_decodes(&thumb_demand, cx);
        self.ensure_library_thumbnails(&thumb_demand, cx);
        // 记录已删除的卡片：解码结果也一并回收，不占 LRU 名额
        self.prune_library_thumb_images(cx);
        // 同理补齐文件大小（每任务只 stat 一次，不再逐帧读盘）
        self.ensure_library_sizes(cx);

        // 页面外壳统一走 primitives::page_shell：内边距 / 分区间距与其余工作台页同源。
        // 注意本页内容自身带滚动列表（library-cards-scroll），外层不再额外滚动，
        // 故把 page_shell 自带的 overflow_y_scroll 顶掉，避免双滚动条。
        primitives::page_shell("library-workspace-layout")
            .overflow_hidden()
            // 顶部标头栏
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(Theme::SPACE_3))
                            .child(primitives::page_title("视频库"))
                            // 计数徽标统一走 badge 原语（不再是手写的圆角胶囊）
                            .child(primitives::badge(format!("{} 项", total_count))),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(Theme::SPACE_2))
                            .child(
                                primitives::btn_clickable(
                                    "导入视频",
                                    primitives::BtnSize::Md,
                                    primitives::BtnVariant::Primary,
                                )
                                .id("library-import-btn")
                                .on_click(cx.listener(
                                    |this, _, _, cx| {
                                        this.state.active_tab = WorkspaceTab::Generate;
                                        cx.notify();
                                    },
                                )),
                            )
                            .child(
                                primitives::btn_clickable(
                                    "刷新",
                                    primitives::BtnSize::Md,
                                    primitives::BtnVariant::Secondary,
                                )
                                .id("library-refresh-btn")
                                .on_click(cx.listener(
                                    |this, _, _, cx| {
                                        this.state.refresh_recent_tasks();
                                        // 列表可能因刷新而变短，顺带裁掉查不到的缓存
                                        this.prune_library_caches();
                                        // 刷新可能只改了一条记录的翻译状态（长度与首条
                                        // id 都没变），而查询缓存的键侦测不到这种变化：
                                        // 不整体作废，筛选结果会继续用旧 id 列表。
                                        this.library_query_cache = None;
                                        cx.notify();
                                    },
                                )),
                            ),
                    ),
            )
            // 检索栏：关键字 + 未翻译筛选 + 排序 + 命中摘要。只有空库时才隐藏。
            .children((total_count > 0).then(|| self.render_library_toolbar(cx)))
            // 批量操作栏：勾选后才展开，不选时不占面积
            .children(
                (!self.library_selected.is_empty()).then(|| self.render_library_batch_bar(cx)),
            )
            // 视频卡片列表区域
            .child(if total_count == 0 {
                self.render_library_empty(cx)
            } else {
                self.render_library_cards(cx)
            })
    }

    /// 视频库检索栏：关键字输入 + 未翻译筛选 + 排序 + 命中摘要。
    ///
    /// # 为什么要有它
    ///
    /// 此前视频库是一条平铺到底的列表，40 条以上只能靠滚动找。用户想找「上个月那门课」
    /// 或「还没翻译的那几个」时没有任何入口——而这两件事恰恰是回头翻库的**唯一**理由。
    ///
    /// # 与「字幕搜索」的区别
    ///
    /// 那个过滤的是**句**，这个过滤的是**记录**；两者互不相干，所以是两个独立缓冲，
    /// 不共用输入框。
    fn render_library_toolbar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        use crate::utils::library_query::{describe, LibrarySort};
        let total = self.state.recent_tasks.len();
        let shown = self.library_visible_ids().len();
        let filter = self.library_filter.clone();
        let filtering = !filter.is_empty();
        let sort = self.library_sort;

        // 关键字输入框：与字幕搜索同一套最小编辑集合（见 `apply_line_edit`）。
        let is_focused = self.library_search_focused;
        let focus = self.library_search_focus.clone();
        let raw = filter.query.clone();
        let char_count = raw.chars().count();
        let cursor = self.library_search_cursor.min(char_count);
        let before: String = raw.chars().take(cursor).collect();
        let after: String = raw.chars().skip(cursor).collect();
        let input = div()
            .id("library-search-input")
            .flex_1()
            .min_w(px(Theme::SEARCH_MIN_W))
            .h(px(Theme::CTRL_H_XS))
            .px(px(Theme::SPACE_2))
            .rounded(px(Theme::RADIUS_MD))
            .bg(Theme::bg_input())
            .border_1()
            // 聚焦描边走 primitives::text_input 的口径（accent_blue）：薄荷绿在本站
            // 专表「我在哪一页」，控件自己的编辑态再用绿会和导航语义撞车。
            .border_color(if is_focused {
                Theme::accent_blue()
            } else {
                Theme::border_mid()
            })
            .cursor_text()
            .flex()
            .items_center()
            .overflow_hidden()
            .track_focus(&focus)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    window.focus(&focus);
                    this.library_search_focused = true;
                    this.library_search_cursor = this.library_filter.query.chars().count();
                    cx.notify();
                }),
            )
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" {
                    this.clear_library_filter(cx);
                    return;
                }
                let buffer = &mut this.library_filter.query;
                let cursor = &mut this.library_search_cursor;
                if apply_line_edit(buffer, cursor, event, cx) {
                    // 关键字变了：缓存必须立刻作废（否则界面还显示旧的命中集合）。
                    this.library_query_cache = None;
                    cx.notify();
                }
            }))
            .child(if is_focused {
                div()
                    .flex()
                    .items_center()
                    .text_size(px(Theme::TEXT_SMALL))
                    .text_color(Theme::text_primary())
                    .child(before)
                    .child(div().text_color(Theme::accent_blue()).child("▌"))
                    .child(after)
                    .into_any_element()
            } else if char_count == 0 {
                div()
                    .text_size(px(Theme::TEXT_SMALL))
                    .text_color(Theme::text_muted())
                    .child("搜索视频…")
                    .into_any_element()
            } else {
                div()
                    .text_size(px(Theme::TEXT_SMALL))
                    .text_color(Theme::text_primary())
                    .truncate()
                    .child(raw)
                    .into_any_element()
            });

        // 状态筛选：未翻译 / 已翻译（互斥，点已选的再点一次即取消）。
        let toggle = |id: &'static str,
                      label: &'static str,
                      on: bool,
                      which: u8,
                      cx: &mut Context<Self>|
         -> Stateful<Div> {
            primitives::segmented(label, on, false)
                .id(id)
                .on_click(cx.listener(move |this, _, _, cx| {
                    match which {
                        1 => {
                            this.library_filter.untranslated_only =
                                !this.library_filter.untranslated_only;
                            if this.library_filter.untranslated_only {
                                this.library_filter.translated_only = false;
                            }
                        }
                        2 => {
                            this.library_filter.translated_only =
                                !this.library_filter.translated_only;
                            if this.library_filter.translated_only {
                                this.library_filter.untranslated_only = false;
                            }
                        }
                        _ => {}
                    }
                    this.library_query_cache = None;
                    cx.notify();
                }))
        };

        // 排序：下拉式一行胶囊（只有 4 项，不值得为它做一个浮层）。
        let sort_row = div()
            .flex()
            .items_center()
            .gap(px(Theme::SPACE_1))
            .child(
                div()
                    .text_size(px(Theme::TEXT_CAPTION))
                    .text_color(Theme::text_muted())
                    .flex_shrink_0()
                    .child("排序"),
            )
            .children(LibrarySort::ALL.into_iter().map(|s| {
                primitives::chip_clickable(s.label(), s == sort, false)
                    .id(SharedString::from(format!("library-sort-{}", s as u8)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.library_sort = s;
                        this.library_query_cache = None;
                        cx.notify();
                    }))
            }));

        div()
            .id("library-toolbar")
            .w_full()
            .mb(px(Theme::SPACE_3))
            .flex()
            .items_center()
            .flex_wrap()
            .gap(px(Theme::SPACE_2))
            .child(
                div()
                    .flex_shrink_0()
                    .text_size(px(Theme::TEXT_SMALL))
                    .text_color(Theme::text_muted())
                    .child("搜索"),
            )
            .child(div().flex_1().min_w(px(Theme::SEARCH_MIN_W)).child(input))
            .child(toggle(
                "library-filter-untranslated",
                "未翻译",
                filter.untranslated_only,
                1,
                cx,
            ))
            .child(toggle(
                "library-filter-translated",
                "已翻译",
                filter.translated_only,
                2,
                cx,
            ))
            .children(filtering.then(|| {
                primitives::chip_clickable("清空筛选", false, false)
                    .id("library-clear-filter")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.clear_library_filter(cx);
                    }))
            }))
            .child(sort_row)
            .child(
                primitives::chip_clickable("全选", false, false)
                    .id("lib-toolbar-select-all")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.library_selected =
                            this.state.recent_tasks.iter().map(|t| t.id).collect();
                        cx.notify();
                    })),
            )
            .child(
                primitives::chip_clickable("查重", false, false)
                    .id("lib-toolbar-find-dupes")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.report_library_duplicates(cx);
                    })),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .font_family("Consolas")
                    .text_size(px(Theme::TEXT_CAPTION))
                    .text_color(if filtering && shown == 0 {
                        Theme::accent_red()
                    } else {
                        Theme::text_muted()
                    })
                    .child(describe(total, shown, &filter)),
            )
            .into_any_element()
    }

    /// 视频库批量操作栏：勾选后展开
    fn render_library_batch_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let selected = self.library_selected.len();
        let total = self.state.recent_tasks.len();
        let busy = self.library_export_busy;
        let can_export = selected > 0 && !busy;
        div()
            .id("library-batch-bar")
            .w_full()
            .px(px(Theme::SPACE_4))
            .py(px(Theme::SPACE_2))
            .mb(px(Theme::SPACE_3))
            .rounded_lg()
            .bg(Theme::bg_raised())
            .border_1()
            .border_color(Theme::tint_mint_border())
            .flex()
            .items_center()
            .justify_between()
            .child(
                div().flex().items_center().gap(px(Theme::SPACE_2)).child(
                    div()
                        .text_size(px(Theme::TEXT_BODY))
                        .font_weight(FontWeight::BOLD)
                        .text_color(Theme::accent_mint())
                        .child(format!("已选 {selected} / {total} 项")),
                ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(Theme::SPACE_2))
                    .child(
                        primitives::chip_clickable("取消全选", false, false)
                            .id("lib-clear-sel-btn")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.library_selected.clear();
                                cx.notify();
                            })),
                    )
                    .child(
                        primitives::btn_state(
                            if busy { "导出中…" } else { "导出选中" },
                            primitives::BtnSize::Sm,
                            primitives::BtnVariant::Primary,
                            can_export,
                        )
                        .id("lib-export-selected-btn")
                        .when(can_export, |d| {
                            d.on_click(cx.listener(|this, _, _, cx| {
                                this.export_selected_library_tasks(cx);
                            }))
                        }),
                    ),
            )
            .into_any_element()
    }

    /// 空状态
    fn render_library_empty(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(Theme::SPACE_3))
            .child(
                div()
                    .text_size(px(Theme::TEXT_HEADING))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(Theme::text_secondary())
                    .child("暂无转写记录"),
            )
            .child(
                div()
                    .text_size(px(Theme::TEXT_BODY_LG))
                    .text_color(Theme::text_muted())
                    .child("导入视频文件后，转写记录将在此显示"),
            )
            .child(
                // 空态主行动按钮走 btn 原语，与其余页面的 CTA 同高同色
                primitives::btn_clickable(
                    "导入音视频开始转写",
                    primitives::BtnSize::Lg,
                    primitives::BtnVariant::Primary,
                )
                .id("empty-lib-goto-gen")
                .mt(px(Theme::SPACE_2))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.state.active_tab = WorkspaceTab::Generate;
                    cx.notify();
                })),
            )
            .into_any_element()
    }

    /// 卡片列表：按筛选/排序结果渲染（仍是借用 `recent_tasks`，不克隆 segments）。
    ///
    /// 顺序来自 [`MainWindow::library_visible_ids`]，它按 `(筛选, 排序, 记录数, 首条 id)`
    /// 缓存——卡片列表没有虚拟化，逐帧重跑 40 次大小写转换的字符串匹配、再每次重排，
    /// 是白白掉帧。
    fn render_library_cards(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let ids = self.library_visible_ids();
        // 命中为空时给一条明确提示：一片空白看起来像「库坏了」而不是「筛掉了」。
        if ids.is_empty() {
            return div()
                .id("library-cards-scroll")
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(Theme::SPACE_2))
                .child(
                    div()
                        .text_size(px(Theme::TEXT_BODY_LG))
                        .text_color(Theme::text_muted())
                        .child("没有符合筛选条件的记录"),
                )
                .child(
                    primitives::chip_clickable("清空筛选", false, false)
                        .id("library-clear-filter-empty")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.clear_library_filter(cx);
                        })),
                )
                .into_any_element();
        }
        // 逐卡片线性 find 是「卡片数 × 记录数」——50 × 50 = 每帧 2500 次比较。
        // 改成每帧建一次「id → 下标」索引再查表，见 `library_ordered_tasks`。
        let ordered = library_ordered_tasks(&self.state.recent_tasks, &ids);
        let cards: Vec<AnyElement> = ordered
            .into_iter()
            .map(|task| self.render_library_card(task, cx).into_any_element())
            .collect();
        // 记录滚动位置与可视子项下标（`ScrollHandle`）：缩略图解码只补视口附近的
        // 卡片，见 `library_thumb_decode_demand`。句柄存在 GPUI 全局里，因为本模块
        // 只能改 `views/library.rs`（`MainWindow` 的字段定义在并发改动中的 ui/mod.rs）。
        let scroll_handle =
            cx.update_default_global::<LibraryThumbState, _>(|state, _| state.scroll.clone());
        div()
            .id("library-cards-scroll")
            .flex_1()
            .overflow_y_scroll()
            .track_scroll(&scroll_handle)
            .flex()
            .flex_col()
            .gap(px(Theme::SPACE_3))
            .children(cards)
            .into_any_element()
    }

    /// 当前可见的记录 id（按筛选 + 排序），带缓存。
    ///
    /// 缓存键含「记录数 + 首条 id」：这两项足以侦测「库被刷新/增删了」。只按记录数会
    /// 漏掉「删一条又加一条」；只按首条 id 会漏掉「只改了列表长度」。
    pub(crate) fn library_visible_ids(&mut self) -> Vec<i64> {
        use crate::utils::library_query::query;
        let key = (
            self.library_filter.query.clone(),
            self.library_filter.translated_only,
            self.library_filter.untranslated_only,
            self.library_sort as u8,
            self.state.recent_tasks.len(),
            self.state.recent_tasks.first().map(|t| t.id).unwrap_or(0),
        );
        if let Some((cached_key, ids)) = self.library_query_cache.as_ref() {
            if cached_key == &key {
                return ids.clone();
            }
        }
        let ids = query(
            &self.state.recent_tasks,
            &self.library_filter,
            self.library_sort,
        );
        self.library_query_cache = Some((key, ids.clone()));
        ids
    }

    /// 清除全部筛选（关键字 + 状态筛选，保留排序——排序是显示偏好，不是筛选）。
    pub(crate) fn clear_library_filter(&mut self, cx: &mut Context<Self>) {
        self.library_filter.query.clear();
        self.library_filter.translated_only = false;
        self.library_filter.untranslated_only = false;
        self.library_search_cursor = 0;
        self.library_query_cache = None;
        cx.notify();
    }

    /// 为视频库卡片补齐「文件大小」显示文本（每个任务只探测一次）。
    ///
    /// 大小要 `fs::metadata`，而卡片渲染每帧都读它；列表又没有虚拟化，
    /// 40 条记录 = 每帧 40 次 stat。因此在载入视频库时探一次并缓存。
    ///
    /// 探测放在后台线程：文件可能在机械盘或网络盘上，`metadata` 卡住时
    /// 不能拖住 UI 线程。失败（文件被删/不可访问）写「未知大小」占位，
    /// 与缩略图失败占位同一策略，避免每次渲染重复派发。
    fn ensure_library_sizes(&mut self, cx: &mut Context<Self>) {
        let pending: Vec<(i64, PathBuf)> = self
            .state
            .recent_tasks
            .iter()
            .filter(|t| !self.library_sizes.contains_key(&t.id))
            .map(|t| (t.id, PathBuf::from(&t.file_path)))
            .collect();
        if pending.is_empty() {
            return;
        }

        cx.spawn(async move |this, cx| {
            let computed = cx
                .background_executor()
                .spawn(async move {
                    pending
                        .into_iter()
                        .map(|(id, path)| {
                            let text = std::fs::metadata(&path)
                                .ok()
                                .map(|m| format_file_size(m.len()))
                                .unwrap_or_else(|| "未知大小".to_string());
                            (id, text)
                        })
                        .collect::<Vec<_>>()
                })
                .await;

            let _ = this.update(cx, |this, cx| {
                for (id, text) in computed {
                    this.library_sizes.insert(id, text);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 为视频库卡片异步预提取首帧缩略图（**帧文件**；解码是下一步的事）。
    ///
    /// 复用全局 FrameCache（磁盘级缓存：路径+文件签名+时间键，跨会话命中），时间 0.0 即首帧；
    /// 每个任务仅派发一次，失败时写入空路径标记防止逐帧重试刷爆后台。
    /// `demand`（视口内 + 上下边距的卡片 id）优先派发：滚动到的卡片要先有帧文件，
    /// 解码才有得做。
    fn ensure_library_thumbnails(&mut self, demand: &[i64], cx: &mut Context<Self>) {
        // 先收集待派发项（只取 id 与路径），避免在借用 recent_tasks 的同时
        // 修改 library_thumb_inflight，也避免克隆整段字幕数据
        let mut pending: Vec<(i64, PathBuf)> = self
            .state
            .recent_tasks
            .iter()
            .filter(|t| {
                !self.library_thumbs.contains_key(&t.id)
                    && !self.library_thumb_inflight.contains(&t.id)
            })
            .map(|t| (t.id, PathBuf::from(&t.file_path)))
            .collect();

        // 视口内的卡片排到最前（稳定排序：其余保持列表原序）
        let priority: HashSet<i64> = demand.iter().copied().collect();
        pending.sort_by_key(|(id, _)| !priority.contains(id));

        for (task_id, video_path) in pending {
            // A render can revisit this method frequently. Bound background
            // ffmpeg work so a large library cannot starve Whisper workers.
            if self.library_thumb_inflight.len() >= MAX_THUMB_WORKERS {
                break;
            }
            if !video_path.exists() {
                continue;
            }
            self.library_thumb_inflight.insert(task_id);

            let ffmpeg = Arc::new(crate::engines::FFmpegEngine::new(
                crate::utils::AppConfig::resolve_path(&self.state.config.paths.ffmpeg),
            ));
            let cache = self.state.frame_cache.clone();

            cx.spawn(async move |this, cx| {
                let thumb = cx
                    .background_executor()
                    .spawn(async move { cache.get_or_extract(&video_path, 0.0, &ffmpeg) })
                    .await;

                let _ = this.update(cx, |this, cx| {
                    this.library_thumb_inflight.remove(&task_id);
                    // 失败（或后台提取出来的文件随后被磁盘裁剪掉）都归一成**空路径**，
                    // 空路径 = 「没有可用缩略图」，渲染时直接走占位框且不再重复派发。
                    // 在这里判一次存在性（后台线程），渲染路径就不必每帧对每张卡片
                    // `path.exists()` —— 库列表没有虚拟化，40 张卡片就是每帧 40 次 stat。
                    let raw = thumb.unwrap_or_default();
                    let usable = if raw.as_os_str().is_empty() || raw.exists() {
                        raw
                    } else {
                        PathBuf::new()
                    };
                    this.library_thumbs.insert(task_id, usable);
                    cx.notify();
                });
            })
            .detach();
        }
    }

    /// 本帧需要**解码结果**的卡片 id（按列表顺序）：视口内 + 上下各
    /// [`LIBRARY_THUMB_PREFETCH_MARGIN`] 张。
    ///
    /// 小库（卡片数 ≤ [`LIBRARY_THUMB_IMAGE_CAP`]）直接整库解码——与改造前的行为
    /// 一致，任何卡片都不闪占位；大库只补视口附近，这正是「50 条库 ≈ 100MB 常驻」
    /// 收敛成「常驻十几到几十 MB」的关键。滚动句柄在首次 prepaint 之前还没有子项
    /// 信息（`children_count()==0`），此时按「列表开头铺到上限」派发，避免首帧空转。
    fn library_thumb_decode_demand(&mut self, cx: &mut Context<Self>) -> Vec<i64> {
        let ids = self.library_visible_ids();
        if ids.is_empty() {
            return Vec::new();
        }
        if ids.len() <= LIBRARY_THUMB_IMAGE_CAP {
            // 小库整库解码：与改造前行为一致，任何卡片都不闪占位。
            // 已解码的会由 `ensure_library_thumb_decodes` 的 contains 检查跳过，
            // 所以这里直接返回全量 id，不需要过滤。
            return ids;
        }
        let (top, bottom, tracked_children) =
            cx.update_default_global::<LibraryThumbState, _>(|state, _| {
                (
                    state.scroll.top_item(),
                    state.scroll.bottom_item(),
                    state.scroll.children_count(),
                )
            });
        let mut demand: Vec<i64> = if tracked_children == 0 {
            // 首帧（句柄还没子项信息）：从列表开头铺到上限，避免首屏空转。
            ids[..LIBRARY_THUMB_IMAGE_CAP.min(ids.len())].to_vec()
        } else {
            ids[thumb_demand_slice(ids.len(), top, bottom, LIBRARY_THUMB_PREFETCH_MARGIN)].to_vec()
        };
        // 兜底：句柄可能长时间拿不到子项（列表尚未测量、或子项还没挂上），
        // 此时 demand 会永远停在「列表前 32 张」，首屏之外的卡片全是占位框。
        // 所以只要还没到上限，就按列表顺序把**尚未解码**的卡片接上——真正的
        // 限流由 `MAX_THUMB_WORKERS`（同时只有 2 个后台任务）负责，这里只是决定
        // 「下一批该解谁」，不会一次把队列打满。
        if demand.len() < LIBRARY_THUMB_IMAGE_CAP {
            let decoded = cx.update_default_global::<LibraryThumbState, _>(|state, _| {
                ids.iter().filter(|id| state.images.contains(**id)).count()
            });
            if decoded < ids.len() {
                let want = LIBRARY_THUMB_IMAGE_CAP - demand.len();
                // 只补**尚未解码**的：已解码的会被 `ensure_library_thumb_decodes` 的
                // contains 检查跳过，把它们算进 demand 只是白占本帧名额，把真正
                // 还没解的卡片挤到下一帧（滚动时表现为「总是慢一拍」）。
                let seen: HashSet<i64> = demand.iter().copied().collect();
                let undecoded = cx.update_default_global::<LibraryThumbState, _>(|state, _| {
                    ids.iter()
                        .filter(|id| !seen.contains(id) && !state.images.contains(**id))
                        .take(want)
                        .copied()
                        .collect::<Vec<i64>>()
                });
                demand.extend(undecoded);
            }
        }
        demand
    }

    /// 为「视口内 + 上下边距」的卡片补派发**后台解码**任务。
    ///
    /// 复用与提取同一份预算（[`MAX_THUMB_WORKERS`] 并发 + `library_thumb_inflight`
    /// 去重），把「已提取到帧文件」的卡片解码成可直接上传纹理的 `RenderImage`。
    /// 解码在后台线程完成（960 宽的 JPG 解一次十几毫秒，放渲染线程就是稳定掉帧），
    /// 交互路径不受影响；失败按帧文件是否还在决定「重来一次」还是「不再重试」。
    fn ensure_library_thumb_decodes(&mut self, demand: &[i64], cx: &mut Context<Self>) {
        for &task_id in demand {
            if self.library_thumb_inflight.len() >= MAX_THUMB_WORKERS {
                break;
            }
            if self.library_thumb_inflight.contains(&task_id) {
                continue;
            }
            if cx.update_default_global::<LibraryThumbState, _>(|state, _| {
                state.images.contains(task_id)
            }) {
                continue;
            }
            // 帧文件路径：空路径 = 上次提取失败。**允许重试**——帧文件存放在全库
            // 共用的 `%TEMP%\\v2w_frames` 里，会被「最近 400 张」的目录裁剪清掉，
            // 上次失败也不代表这次还失败（ffmpeg 可能刚被修好路径）。真正的防刷
            // 爆由 `MAX_THUMB_WORKERS` 限流 + `library_thumb_inflight` 去重负责。
            let Some(frame_path) = self.library_thumbs.get(&task_id).cloned() else {
                continue;
            };
            if frame_path.as_os_str().is_empty() {
                // 上次失败：本帧只放行「还在视口需求里」的卡片，避免全库同时重抽。
                if !demand.contains(&task_id) {
                    continue;
                }
                self.library_thumbs.remove(&task_id);
            }
            self.library_thumb_inflight.insert(task_id);

            cx.spawn(async move |this, cx| {
                let (decoded, frame_path) = cx
                    .background_executor()
                    .spawn(async move {
                        let decoded = crate::ui::editor::render_image_from_frame_file(&frame_path);
                        (decoded, frame_path)
                    })
                    .await;

                let _ = this.update(cx, |this, cx| {
                    this.library_thumb_inflight.remove(&task_id);
                    if let Some(image) = decoded {
                        let stale = cx.update_default_global::<LibraryThumbState, _>(|state, _| {
                            state.images.insert(task_id, image)
                        });
                        if !stale.is_empty() {
                            // 显式回收换下来的旧图：GPUI 精灵图集只按 image_id 释放
                            // （`Window::drop_image`），没人调就永远常驻显存。本渲染
                            // 链路拿不到 `&mut Window`（`render_library_layout` 的调用
                            // 点没有窗口参数），所以走 `defer`：回调在本轮效果处理末尾
                            // 执行，此时所有窗口都不在栈上，`App::drop_image` 会遍历
                            // 每个窗口把纹理删掉（gpui 0.2.2）。
                            cx.defer(move |cx| {
                                for image in stale {
                                    cx.drop_image(image, None);
                                }
                            });
                        }
                    } else if frame_path.exists() {
                        // 帧文件还在却解不出来（损坏）：按提取失败同一策略记空路径，
                        // 不再逐帧重试；渲染回到占位框。
                        this.library_thumbs.insert(task_id, PathBuf::new());
                    } else {
                        // 帧文件已被磁盘裁剪：丢掉路径，下一帧重新提取一帧再解码（自愈）。
                        this.library_thumbs.remove(&task_id);
                    }
                    cx.notify();
                });
            })
            .detach();
        }
    }

    /// 清掉记录已被删除的卡片解码结果（`library_thumbs` 是删除/刷新后 `retain` 过的
    /// 真相来源），回收同样走 `defer` + `drop_image`。
    fn prune_library_thumb_images(&mut self, cx: &mut Context<Self>) {
        if cx.update_default_global::<LibraryThumbState, _>(|state, _| state.images.is_empty()) {
            return;
        }
        let live: HashSet<i64> = self.library_thumbs.keys().copied().collect();
        let stale = cx.update_default_global::<LibraryThumbState, _>(|state, _| {
            state.images.remove_missing(&live)
        });
        if stale.is_empty() {
            return;
        }
        cx.defer(move |cx| {
            for image in stale {
                cx.drop_image(image, None);
            }
        });
    }

    /// 单条视频卡片 —— 横向布局：缩略图 | 信息区 | 操作按钮
    /// 只借用任务记录，点击时再按 id 回查，避免每次渲染克隆整段字幕
    fn render_library_card(
        &self,
        task: &crate::storage::db::TaskRecord,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let task_id = task.id;
        let seg_len = task.segment_count;
        let dur_str = format_duration_short(task.duration);

        // 文件扩展名
        let ext = library_ext_label(&task.file_name);

        // 文件大小：查**缓存快照**，不在这里 stat。
        //
        // 这个函数每个可见卡片每帧都会被调一次，而视频库列表是普通
        // `overflow_y_scroll` + `children`（没有虚拟化），40 条记录就是
        // 每帧 40 次 `fs::metadata` —— 磁盘慢或文件在网络盘上时直接表现为
        // 滚动卡顿。大小只在「载入视频库」时探测一次（见 `ensure_library_sizes`）。
        let file_size_str = self
            .library_sizes
            .get(&task_id)
            .cloned()
            .unwrap_or_else(|| "未知大小".to_string());

        // 摘录首条字幕文本：列表查询已由 SQLite 抽出首句文本，这里不再反序列化整段字幕。
        // 截断规则见 `library_sample_preview`（按字符边界，避免切进多字节中文字符）。
        let sample_text = library_sample_preview(&task.sample_text, LIBRARY_SAMPLE_MAX_CHARS);

        // 格式化日期（取日期部分）
        // 取日期部分（前 10 字节 = "YYYY-MM-DD"）。这里用 `get(..10)` 而不是直接
        // `[..10]` 切片：`created_at` 正常由数据库写入固定为 ASCII 时间戳，但万一
        // 混入多字节字符、且第 10 字节恰好落在字符中间，直接切片会 panic，`get` 则
        // 安全返回 None 并退化为显示整串。
        let date_display = task
            .created_at
            .get(..10)
            .map(str::to_string)
            .unwrap_or_else(|| task.created_at.clone());

        let selected = self.library_selected.contains(&task_id);
        div()
            .id(("lib-card", task_id as usize))
            .w_full()
            .p(px(Theme::SPACE_4))
            .rounded_xl()
            .bg(Theme::bg_card())
            .border_1()
            .border_color(if selected {
                Theme::accent_mint()
            } else {
                Theme::border()
            })
            .hover(|s| {
                s.border_color(Theme::border_light())
                    .bg(Theme::bg_card_hover())
            })
            .flex()
            .flex_row()
            .items_center()
            .gap(px(Theme::SPACE_4))
            // ── 最左：勾选框（批量导出用） ──
            .child(
                div()
                    .id(("lib-check", task_id as usize))
                    .flex_shrink_0()
                    .w(px(20.0))
                    .h(px(20.0))
                    .rounded(px(Theme::RADIUS_MD))
                    .border_1()
                    .border_color(if selected {
                        Theme::accent_mint()
                    } else {
                        Theme::border_mid()
                    })
                    .bg(if selected {
                        Theme::accent_mint()
                    } else {
                        Theme::bg_raised()
                    })
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .text_size(px(Theme::TEXT_SMALL))
                    .font_weight(FontWeight::BOLD)
                    .text_color(Theme::text_on_accent())
                    .hover(|s| s.border_color(Theme::accent_mint()))
                    .child(if selected { "✓" } else { "" })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        // insert 返回 false 表示原本已在集合里 → 再点即取消勾选
                        if !this.library_selected.insert(task_id) {
                            this.library_selected.remove(&task_id);
                        }
                        cx.notify();
                    })),
            )
            // ── 左侧：首帧缩略图（后台提取完成前显示占位） ──
            .child({
                let thumb_box = div()
                    .flex_shrink_0()
                    .w(px(144.0))
                    .h(px(81.0))
                    .rounded_lg()
                    .overflow_hidden()
                    .bg(Theme::bg_sidebar())
                    .border_1()
                    .border_color(Theme::border_subtle())
                    .relative();

                // 缩略图不再走 `img(路径)`：GPUI 会把每张解码结果永久留在资产缓存里
                // （`App::fetch_asset` 只增不删，50 条库 ≈ 100MB）。改渲染后台线程
                // 解码好的 `RenderImage`（见 `ensure_library_thumb_decodes`），内存里
                // 只留最近使用的 [`LIBRARY_THUMB_IMAGE_CAP`] 张。
                let has_frame = self
                    .library_thumbs
                    .get(&task_id)
                    .is_some_and(|p| !p.as_os_str().is_empty());
                let thumb_image = if has_frame {
                    cx.update_default_global::<LibraryThumbState, _>(|state, _| {
                        state.images.get(task_id).cloned()
                    })
                } else {
                    None
                };
                match thumb_image {
                    Some(image) => thumb_box
                        // 视频首帧铺满卡片，Cover 裁剪对齐 16:9
                        .child(
                            img(ImageSource::Render(image))
                                .size_full()
                                .object_fit(ObjectFit::Cover),
                        )
                        // 底部信息条：格式 + 时长
                        .child(
                            div()
                                .absolute()
                                .bottom_0()
                                .left_0()
                                .right_0()
                                .flex()
                                .items_center()
                                .justify_between()
                                .px(px(Theme::SPACE_2))
                                .py(px(Theme::SPACE_0_5))
                                // 角标条压在缩略图上，底色恒为深色，文字必须用媒体区浅色 token
                                // （浅色主题下 text_muted 会变深，压在深底上直接糊掉）
                                .bg(Theme::bg_overlay())
                                .child(
                                    div()
                                        .text_size(px(Theme::TEXT_CAPTION))
                                        .font_weight(FontWeight::BOLD)
                                        .text_color(Theme::text_on_overlay())
                                        .child(ext),
                                )
                                .child(
                                    div()
                                        .text_size(px(Theme::TEXT_CAPTION))
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(Theme::text_on_overlay())
                                        .child(dur_str.clone()),
                                ),
                        ),
                    None => thumb_box
                        .flex()
                        .items_center()
                        .justify_center()
                        .flex_col()
                        .gap(px(Theme::SPACE_1))
                        // 格式标识
                        .child(
                            div()
                                .px(px(Theme::SPACE_3))
                                .py(px(Theme::SPACE_1))
                                .rounded_md()
                                .bg(Theme::bg_raised())
                                .border_1()
                                .border_color(Theme::border_mid())
                                .text_size(px(Theme::TEXT_BODY_LG))
                                .font_weight(FontWeight::BOLD)
                                .text_color(Theme::text_muted())
                                .child(ext),
                        )
                        // 时长标签
                        .child(
                            div()
                                .text_size(px(Theme::TEXT_SMALL))
                                .text_color(Theme::text_muted())
                                .child(dur_str.clone()),
                        ),
                }
            })
            // ── 中间：信息区 ──
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .flex()
                    .flex_col()
                    .justify_between()
                    .gap(px(Theme::SPACE_2_5))
                    // 文件名 + 状态
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(Theme::SPACE_2))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(Theme::SPACE_2_5))
                                    // 文件名 (大字)
                                    .child(
                                        div()
                                            .text_size(px(Theme::TEXT_HEADING))
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(Theme::text_primary())
                                            .overflow_hidden()
                                            .child(task.file_name.clone()),
                                    )
                                    // 已完成标记
                                    .child(primitives::stat_dot(Theme::accent_mint())),
                            )
                            // 元数据标签行：极简点号分隔
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(Theme::SPACE_2))
                                    .text_size(px(Theme::TEXT_SMALL))
                                    .text_color(Theme::text_muted())
                                    .child(dur_str)
                                    .child("·")
                                    .child(file_size_str)
                                    .child("·")
                                    .child(format!("{} 句", seg_len))
                                    .children(task.metrics.as_ref().map(|m| {
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap(px(Theme::SPACE_2))
                                            .child("·")
                                            .child(format!("{:.1}s", m.total_elapsed_sec))
                                    }))
                                    .child("·")
                                    .child(date_display),
                            ),
                    )
                    // 字幕预览文本
                    .child(
                        div()
                            .text_size(px(Theme::TEXT_BODY))
                            .text_color(Theme::text_secondary())
                            .truncate()
                            .child(sample_text),
                    ),
            )
            // ── 右侧：操作按钮组 ──
            .child(
                div()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap(px(Theme::SPACE_2))
                    // 耗时详情 (if metrics)
                    // 与「校对 / 导出」同法：渲染期只判断「有没有指标」，点开那一刻才把
                    // 那一条的指标取出来。这里原先每帧对每张卡片 `clone()` 一次 metrics
                    // （十来个 f64 的聚合结构），50 条库就是每帧 50 次纯浪费的分配——
                    // 而这些数字只有用户点「耗时」时才真正被读到。
                    .children(task.metrics.as_ref().map(|_| {
                        primitives::btn_mint_soft("耗时")
                            .id(("lib-metrics-btn", task_id as usize))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let Some((metrics, file_name)) = this
                                    .state
                                    .recent_tasks
                                    .iter()
                                    .find(|t| t.id == task_id)
                                    .and_then(|t| {
                                        t.metrics.clone().map(|m| (m, t.file_name.clone()))
                                    })
                                else {
                                    return;
                                };
                                this.benchmark_dialog =
                                    Some(crate::ui::types::BenchmarkDialogInfo {
                                        file_name,
                                        metrics,
                                    });
                                cx.notify();
                            }))
                    }))
                    // 剪辑按钮 (主操作)
                    .child(
                        primitives::btn_clickable(
                            "校对",
                            primitives::BtnSize::Md,
                            primitives::BtnVariant::Primary,
                        )
                        .id(("lib-edit-btn", task_id as usize))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            // 切工程前先把当前这条的未落库编辑写回：落库是 1.2s 去抖的，
                            // 用户改完字幕立刻回库里点「校对」，最后一段编辑会连同脏位一起
                            // 被下一条工程顶掉。写失败时拦下这次切换（横幅会说明原因），
                            // 与关窗守卫同一取舍。
                            if !this.flush_segments_before_close(cx) {
                                return;
                            }
                            // 点击时才按 id 取出完整任务数据（渲染期不再克隆）
                            let Some(record) = this
                                .state
                                .recent_tasks
                                .iter()
                                .find(|t| t.id == task_id)
                                .cloned()
                            else {
                                return;
                            };
                            this.state.load_task(&record);
                            this.trigger_extract_frame(cx);
                            this.ensure_preview_proxy(cx);
                            this.ensure_waveform(cx);
                            cx.notify();
                        })),
                    )
                    // 导出按钮：次级中性按钮（抬升底 + 描边），方角
                    .child(
                        primitives::btn_clickable(
                            "导出字幕",
                            primitives::BtnSize::Md,
                            primitives::BtnVariant::Secondary,
                        )
                        .id(("lib-export-btn", task_id as usize))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            // 同上：仅在真正导出时才取出该任务的字幕数据
                            let Some(record) = this
                                .state
                                .recent_tasks
                                .iter()
                                .find(|t| t.id == task_id)
                                .cloned()
                            else {
                                return;
                            };
                            let task_name = record.file_name.clone();
                            // 列表记录不含字幕正文，导出时按 id 现取，避免为一次导出
                            // 把整库字幕都留在内存里
                            let segs = this
                                .state
                                .db
                                .load_task_segments(task_id)
                                .unwrap_or_default();
                            // 与剪辑台同源：尊重用户在导出栏选的「原文 / 仅译文 / 双语」。
                            // 此前这里写死用 `write_to_file` 自动判定，用户改成「仅译文」
                            // 后从历史库导出仍会带上原文，两处行为不一致。
                            let export_mode = this.state.export_mode_from_config();
                            // 导出格式跟随配置抽屉的「字幕输出格式」，不再写死 SRT：
                            // 扩展名 / 对话框过滤器 / 写出格式三者同源（都来自
                            // `writer::export_spec_for`），否则会出现「文件叫 .json、
                            // 里面却是 SRT」这类不一致。
                            // 文件名后缀取**扩展名**，写盘参数取**规范格式名**：两者不是
                            // 一回事——`txt-ts`（带时间码纯文本）的扩展名就是 `txt`，拿扩展名
                            // 当格式名会把「带时间码」静默写成一堆纯文本。
                            let write_fmt = canonical_write_format(&this.state.output_format);
                            let (fmt_ext, fmt_label) = export_spec_for(write_fmt);
                            // 与剪辑台同源：把主界面配置的字幕样式一并带进写出链路，
                            // 否则 SRT/VTT/ASS 不会按「单行最大字数」折行，同一份字幕
                            // 从剪辑台导出会折行、从历史库单条导出却不折，两处不一致。
                            // 这里是 `cx.listener` 的同步闭包，`this.state` 可以直接取；
                            // 拿到值再 move 进异步块（借用活不过 `'static` 的 task）。
                            let style = this.state.config.subtitle_style.clone();
                            cx.spawn(async move |this, cx| {
                                if let Some(handle) = rfd::AsyncFileDialog::new()
                                    .set_file_name(format!("{}.{}", task_name, fmt_ext))
                                    .add_filter(fmt_label, &[fmt_ext])
                                    .save_file()
                                    .await
                                {
                                    let save_path = handle.path().to_path_buf();
                                    // 带样式的入口：srt / vtt / ass 折行，其余格式
                                    // （json / ttml / ttal / txt）在内部原样回落到
                                    // `write_to_file_with_mode`，不会改变原有字节与可用性。
                                    if let Err(err) = SubtitleWriter::write_to_file_with_style(
                                        &segs,
                                        &save_path,
                                        write_fmt,
                                        export_mode,
                                        &style,
                                    ) {
                                        // 导出失败必须报出来：静默吞掉会让用户以为
                                        // 文件已经写出去了，回头找不到又无从排查。
                                        let _ = this.update(cx, |this, cx| {
                                            this.state.status =
                                                ProcessStatus::Failed(format!("导出失败：{err}"));
                                            cx.notify();
                                        });
                                    }
                                }
                            })
                            .detach();
                        })),
                    )
                    // 删除按钮：危险操作按钮原语，卡片级尺寸（32px 圆角块）
                    .child({
                        let task_name = task.file_name.clone();
                        primitives::btn_danger("删除", primitives::BtnSize::Md)
                            .id(("lib-del-btn", task_id as usize))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                // 删除不可逆，且若删的是当前工程会静默换掉工作区，
                                // 必须先确认。真正执行走 ConfirmAction。
                                this.confirm_dialog = Some(ConfirmDialogInfo {
                                    title: "删除这条记录？".to_string(),
                                    message: format!(
                                        "「{task_name}」及其字幕将被永久删除，无法恢复。"
                                    ),
                                    confirm_label: "删除".to_string(),
                                    danger: true,
                                    action: ConfirmAction::DeleteTaskRecord(task_id),
                                });
                                cx.notify();
                            }))
                    }),
            )
    }
}

/// 每帧构建一次的「任务 id → 下标」索引。
///
/// 卡片列表没有虚拟化：50 张卡片各自 `recent_tasks.iter().find(...)` 就是每帧
/// 2500 次比较，纯属白跑。重复 id 保留**首次**出现的下标，与旧线性 `find` 的
/// 语义逐字一致（真实数据受 DB 主键约束不会重复，这里只是不让重构引入差异）。
fn library_task_index(tasks: &[crate::storage::TaskRecord]) -> HashMap<i64, usize> {
    let mut index: HashMap<i64, usize> = HashMap::with_capacity(tasks.len());
    for (i, task) in tasks.iter().enumerate() {
        index.entry(task.id).or_insert(i);
    }
    index
}

/// 按可见 id 的顺序取出任务引用。
///
/// 查不到的 id **跳过**（与旧 `filter_map` 一致：那张卡片不渲染，而不是显示占位），
/// 输出顺序严格跟随 `visible_ids`。
fn library_ordered_tasks<'a>(
    tasks: &'a [crate::storage::TaskRecord],
    visible_ids: &[i64],
) -> Vec<&'a crate::storage::TaskRecord> {
    let index = library_task_index(tasks);
    visible_ids
        .iter()
        .filter_map(|id| index.get(id).map(|i| &tasks[*i]))
        .collect()
}

/// 把字节数格式化成界面用的体积文本。
///
/// 抽成独立函数是因为它现在跑在后台线程里（见 `ensure_library_sizes`），
/// 与渲染路径解耦，也便于单测。
fn format_file_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else {
        format!("{:.0} KB", b / KB)
    }
}

/// 从文件名推断库卡片角标上的扩展名。
///
/// `rsplit + next` 对「只有点号」的文件名会拿到空串、对空名字渲染成空白；
/// 这里跳过空片段并统一回退成 MP4，抽成纯函数便于单测。
fn library_ext_label(file_name: &str) -> String {
    file_name
        .rsplit('.')
        .find(|s| !s.is_empty())
        .unwrap_or("MP4")
        .to_uppercase()
}

/// 视频库缩略图**解码结果**的内存上限（张）。
///
/// 卡片改走自解码的 `RenderImage` 之前，GPUI 的资产缓存（`App::fetch_asset`）只增
/// 不删：一张 960×540 的帧解码后 ≈ 2MB RGBA，50 条库就是 100MB，而且随新建/刷新
/// 只涨不降。这里按「最近使用的 N 张」封顶，换下来的整张交还 GPUI 回收：
/// - 32 张 ≈ 64MB 上限；实际常驻的只是「视口内 + 上下边距」的十几张（≈ 20~30MB），
///   因为解码是**按可视范围按需派发**的（见 `library_thumb_decode_demand`）；
/// - 32 张也覆盖 4K 全屏一屏（卡片高约 100px，2160px 视口 ≈ 22 张）再留一屏滚动余量，
///   避免「滚动到哪都要重解码」；
/// - 卡片总数 ≤ 上限的小库仍然整库解码（与改造前行为一致，不闪占位）。
const LIBRARY_THUMB_IMAGE_CAP: usize = 32;

/// 卡片字幕摘录的最大显示字数（字符数，不是字节数）。
const LIBRARY_SAMPLE_MAX_CHARS: usize = 40;

/// 卡片上的字幕摘录：空内容给占位符，超长按**字符**边界截断并补省略号。
///
/// 按字符而不是按字节截断：中文字幕按字节切会切进多字节字符，轻则最后一个字变
/// 乱码，重则 panic——而卡片是每帧都渲染的地方。截断处补「…」而不是三连点，
/// 与全站其它进行中/截断文案保持同一种记号。
fn library_sample_preview(sample_text: &str, max_chars: usize) -> String {
    if sample_text.trim().is_empty() {
        return "无字幕内容".to_string();
    }
    if sample_text.chars().count() > max_chars {
        let cut: String = sample_text.chars().take(max_chars).collect();
        return format!("{}…", cut);
    }
    sample_text.to_string()
}

/// 视口上下各多解码几张：滚动时下一屏的卡片已经就绪，不至于边滚边闪占位。
const LIBRARY_THUMB_PREFETCH_MARGIN: usize = 4;

/// 缩略图管线（提取 + 解码）共用的后台并发上限。
///
/// 渲染每帧都可能重入派发路径，必须封顶：大库的缩略图工作不能把 Whisper 的 CPU
/// 抢光。提取与解码共用同一份预算（`library_thumb_inflight` 是同一个去重集合）。
const MAX_THUMB_WORKERS: usize = 2;

/// 「最近使用的 N 张」LRU。
///
/// 命中/插入都把键移到队尾，超上限时淘汰队头；`insert`/`remove_missing` 把被淘汰的
/// 负载**返回**给调用方——GPUI 的精灵图集只认 `drop_image` 这一条回收路径，丢之前
/// 必须显式交还（见 `render_monitor_picture` / `cached_static_image` 的注释）。
///
/// 负载做成泛型只为单测：用哨兵值即可钉死「上限 / 命中提升 / 替换回收」语义，
/// 不必为了测 LRU 去构造真实的 `RenderImage`（生产实例是 `Arc<RenderImage>`）。
struct ThumbImageLru<T> {
    cap: usize,
    map: HashMap<i64, T>,
    /// 最近使用的在队尾
    order: VecDeque<i64>,
}

impl<T> ThumbImageLru<T> {
    fn new(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            map: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    fn contains(&self, id: i64) -> bool {
        self.map.contains_key(&id)
    }

    /// 命中并把该卡片提升为「最近使用」。
    fn get(&mut self, id: i64) -> Option<&T> {
        if !self.map.contains_key(&id) {
            return None;
        }
        self.touch(id);
        self.map.get(&id)
    }

    fn touch(&mut self, id: i64) {
        self.order.retain(|k| *k != id);
        self.order.push_back(id);
    }

    /// 插入/替换一张解码结果，返回**需要调用方显式回收**的旧图：
    /// 同一 id 被重新解码替换下来的旧图，以及超上限淘汰的最久未用项。
    fn insert(&mut self, id: i64, value: T) -> Vec<T> {
        self.touch(id);
        let mut stale = Vec::new();
        if let Some(old) = self.map.insert(id, value) {
            stale.push(old);
        }
        while self.map.len() > self.cap {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(old) = self.map.remove(&oldest) {
                stale.push(old);
            }
        }
        stale
    }

    /// 清掉已经不在记录列表里的卡片（记录被删除/库被刷新），返回需要显式回收的图。
    fn remove_missing(&mut self, live: &HashSet<i64>) -> Vec<T> {
        let gone: Vec<i64> = self
            .map
            .keys()
            .copied()
            .filter(|id| !live.contains(id))
            .collect();
        let mut stale = Vec::new();
        for id in gone {
            self.order.retain(|k| *k != id);
            if let Some(old) = self.map.remove(&id) {
                stale.push(old);
            }
        }
        stale
    }
}

/// 视频库缩略图的跨帧状态：解码结果 LRU + 卡片列表的滚动句柄。
///
/// 存在 GPUI 全局（`App::update_default_global`）而不是 `MainWindow` 字段里：本模块
/// 按并发任务边界只能改 `views/library.rs`，而 `MainWindow` 的结构体定义在
/// 并发改动中的 `ui/mod.rs`。全局随 App 生命周期存活，等价于视图级状态。
struct LibraryThumbState {
    images: ThumbImageLru<Arc<RenderImage>>,
    /// 卡片列表滚动容器（`.track_scroll`）的句柄：用它拿到**可视首尾下标**，
    /// 解码派发只补视口附近，不再整库常驻。
    scroll: ScrollHandle,
}

impl Default for LibraryThumbState {
    fn default() -> Self {
        Self {
            images: ThumbImageLru::new(LIBRARY_THUMB_IMAGE_CAP),
            scroll: ScrollHandle::new(),
        }
    }
}

impl Global for LibraryThumbState {}

/// 「可视首尾下标 + 上下边距」→ 要解码的下标区间。
///
/// 抽成纯函数便于单测：`top`/`bottom` 来自滚动句柄（列表为空或尺寸未定时都可能是 0），
/// 返回区间保证落在 `0..len` 内，且列表非空时至少含一张卡片——派发循环靠它保证
/// 「视口里的卡片一定会被解码」。
fn thumb_demand_slice(len: usize, top: usize, bottom: usize, margin: usize) -> Range<usize> {
    if len == 0 {
        return 0..0;
    }
    let last = len - 1;
    let top = top.min(last);
    let bottom = bottom.max(top).min(last);
    let start = top.saturating_sub(margin);
    let end = bottom.saturating_add(margin).saturating_add(1).min(len);
    start..end
}

#[cfg(test)]
mod tests {
    use super::{
        export_spec_for, format_file_size, library_ext_label, library_ordered_tasks,
        library_sample_preview, library_task_index, thumb_demand_slice, ThumbImageLru,
        LIBRARY_THUMB_IMAGE_CAP,
    };
    use crate::storage::TaskRecord;
    use std::collections::HashSet;

    /// 兜底需求必须把「尚未解码」的卡片接上（句柄失灵时首屏外全不解码的回归）。
    ///
    /// 只验证「补哪些」这个纯逻辑：用 `ThumbImageLru<u32>` 当占位，不需要真的造图。
    #[test]
    fn thumb_demand_fallback_covers_undecoded_cards() {
        let ids: Vec<i64> = (0..40).collect();
        let mut decoded_ids: ThumbImageLru<u32> = ThumbImageLru::new(LIBRARY_THUMB_IMAGE_CAP);
        for id in ids.iter().take(5) {
            decoded_ids.insert(*id, 1);
        }

        // 句柄失灵：视口切片只覆盖中间一小段（10..22），未满上限，兜底才该补。
        // （首帧的 0..32 分支本来就等于上限，不需要兜底——那是刻意行为。）
        let mut demand: Vec<i64> = ids[10..22].to_vec();
        if demand.len() < LIBRARY_THUMB_IMAGE_CAP {
            let want = LIBRARY_THUMB_IMAGE_CAP - demand.len();
            let seen: HashSet<i64> = demand.iter().copied().collect();
            let undecoded: Vec<i64> = ids
                .iter()
                .filter(|id| !seen.contains(id) && !decoded_ids.contains(**id))
                .take(want)
                .copied()
                .collect();
            demand.extend(undecoded);
        }

        assert_eq!(
            demand.len(),
            LIBRARY_THUMB_IMAGE_CAP,
            "兜底要把需求补到上限"
        );
        assert!(
            demand.contains(&5) && demand.contains(&22),
            "未解码的卡片要按列表顺序接上（5 = 首个未解码，22 = 视口之后第一个）"
        );
        for id in 10..22i64 {
            assert!(demand.contains(&id), "视口切片里的 {id} 一张都不能少");
        }
    }

    /// 兜底只补**尚未解码**的：已解码的占名额会把真正没解的挤到下一帧，
    /// 滚动时表现为「总是慢一拍」。这条把那个不变量钉住。
    #[test]
    fn thumb_demand_fallback_skips_already_decoded_cards() {
        let ids: Vec<i64> = (0..40).collect();
        let mut decoded_ids: ThumbImageLru<u32> = ThumbImageLru::new(LIBRARY_THUMB_IMAGE_CAP);
        for id in ids.iter().take(40) {
            decoded_ids.insert(*id, 1);
        }
        // 全部已解码 → 兜底什么都不补
        let mut demand: Vec<i64> = ids[..LIBRARY_THUMB_IMAGE_CAP].to_vec();
        if demand.len() < LIBRARY_THUMB_IMAGE_CAP {
            let want = LIBRARY_THUMB_IMAGE_CAP - demand.len();
            let seen: HashSet<i64> = demand.iter().copied().collect();
            let undecoded: Vec<i64> = ids
                .iter()
                .filter(|id| !seen.contains(id) && !decoded_ids.contains(**id))
                .take(want)
                .copied()
                .collect();
            demand.extend(undecoded);
        }
        assert_eq!(
            demand.len(),
            LIBRARY_THUMB_IMAGE_CAP,
            "全已解码时维持视口切片"
        );
    }
    /// 卡片摘录的三种形态：空内容给占位、超长按字符边界截断并补省略号、短句原样。
    /// 省略号必须是单个「…」：三连点在本站其它文案里已被统一掉，这里不能再冒出来。
    #[test]
    fn library_sample_preview_keeps_full_sentences_and_truncates_long_ones() {
        assert_eq!(library_sample_preview("   ", 40), "无字幕内容");
        assert_eq!(library_sample_preview("短句", 40), "短句");
        // 正好等于上限：不截断、不加省略号
        let exact = "一".repeat(40);
        assert_eq!(library_sample_preview(&exact, 40), exact);
        // 超出一个字：截到上限再补省略号，总长 = 上限 + 1
        let long = "一".repeat(41);
        let out = library_sample_preview(&long, 40);
        assert_eq!(out.chars().count(), 41, "40 个字 + 省略号");
        assert!(out.ends_with('…'), "截断要留省略号，让用户知道还有后半句");
        assert!(!out.contains("..."), "三连点写法不该出现在界面上");
    }

    #[test]
    fn file_size_formatting_matches_previous_rendering() {
        // 与迁移前的渲染逻辑逐字对齐，避免「顺手重构」改变界面观感
        assert_eq!(format_file_size(1_073_741_824), "1.0 GB");
        assert_eq!(format_file_size(2_147_483_648), "2.0 GB");
        assert_eq!(format_file_size(1_048_576), "1.0 MB");
        assert_eq!(format_file_size(66_359), "65 KB");
        assert_eq!(format_file_size(1024), "1 KB");
        assert_eq!(format_file_size(0), "0 KB");
    }

    /// 回归（旧 bug）：批量「导出选中」曾把扩展名与写出格式**双双写死成 SRT**，
    /// 用户在配置里选了 JSON，批量导出出来的仍是 `.srt`。
    /// 现在批量导出与单条导出共用同一条构造路径：`export_spec_for(配置值)`
    /// 同时决定文件名后缀与 `write_to_file_with_mode` 的格式参数。
    /// 这里断言这份「构造路径」与配置项一致、且扩展名等于格式名，
    /// 也就是批量导出循环里每个文件的写法都能被 `writer` 接受。
    #[test]
    fn batch_export_extension_follows_selected_format() {
        // 与 `export_selected_library_tasks` 内部逐字一致的构造：扩展名与格式名同源
        for fmt in ["srt", "ass", "vtt", "txt", "json", "ttml", "ttal"] {
            let (ext, label) = export_spec_for(fmt);
            assert_eq!(ext, fmt, "{fmt} 的批量导出扩展名应与格式名一致");
            assert!(!label.is_empty(), "{fmt} 的批量导出需要对话框过滤器名");
            // 旧实现：out = dir.join(format!("{stem}.srt")) + write(..., "srt", ...)
            // 因此「选了 json 却得到 .srt」；现在两者都取自同一份 spec。
            let out = std::path::Path::new("dir").join(format!("{}.{}", "clip", ext));
            assert_eq!(
                out.extension().and_then(|e| e.to_str()),
                Some(fmt),
                "{fmt} 的批量导出文件名后缀不对: {}",
                out.display()
            );
        }
    }

    /// 回归（P2）：文件名扩展名的角标回退——没有扩展名、空名字或只有点号时
    /// 统一回退成 MP4；正常扩展名一律大写显示。
    #[test]
    fn library_extension_falls_back_when_missing() {
        assert_eq!(library_ext_label("clip.mp4"), "MP4");
        assert_eq!(library_ext_label("a.b.mkv"), "MKV");
        assert_eq!(library_ext_label("noext"), "NOEXT");
        assert_eq!(library_ext_label(""), "MP4");
        assert_eq!(library_ext_label("..."), "MP4");
    }
    /// 测试用的最小任务记录（视频库列表返回的就是这种「空壳」记录）。
    fn task_record(id: i64, file_name: &str) -> TaskRecord {
        TaskRecord {
            id,
            file_path: format!("D:/videos/{file_name}"),
            file_name: file_name.to_string(),
            duration: 1.0,
            status: "completed".to_string(),
            segments: Vec::new(),
            segment_count: 0,
            sample_text: String::new(),
            segments_loaded: false,
            has_translation: false,
            created_at: "2026-01-01 00:00:00".to_string(),
            content_hash: None,
            metrics: None,
        }
    }

    /// 回归（P3）：卡片列表从「每张卡片各自线性 `find`」换成每帧一次的
    /// HashMap 索引后，结果必须与旧的 `filter_map(|id| tasks.iter().find(...))`
    /// 逐项相同——顺序跟随可见 id、查不到的 id **跳过**（那张卡片不渲染，
    /// 而不是显示占位）、重复 id 取**首次**出现的记录。
    #[test]
    fn library_index_lookup_matches_linear_find() {
        let tasks = vec![
            task_record(7, "a.mp4"),
            task_record(5, "b.mp4"),
            task_record(3, "c.mp4"),
            // 真实数据受 DB 主键约束 id 唯一；这里故意造一条重复，
            // 钉住「首个取胜」这条旧语义
            task_record(5, "b-dup.mp4"),
        ];
        let visible = vec![3, 7, 99, 5];

        let got: Vec<(i64, String)> = library_ordered_tasks(&tasks, &visible)
            .iter()
            .map(|t| (t.id, t.file_name.clone()))
            .collect();
        let expected: Vec<(i64, String)> = visible
            .iter()
            .filter_map(|id| tasks.iter().find(|t| t.id == *id))
            .map(|t| (t.id, t.file_name.clone()))
            .collect();

        assert_eq!(got, expected, "索引查表必须与旧的逐条线性 find 等价");
        assert_eq!(
            got,
            vec![
                (3, "c.mp4".to_string()),
                (7, "a.mp4".to_string()),
                (5, "b.mp4".to_string())
            ],
            "顺序跟随可见 id；查不到的跳过；重复 id 取首次出现"
        );
        assert_eq!(
            library_task_index(&tasks).get(&5).copied(),
            Some(1),
            "重复 id 必须保留首次出现的下标"
        );
        assert!(library_ordered_tasks(&tasks, &[]).is_empty());
        assert!(
            library_ordered_tasks(&tasks, &[123]).is_empty(),
            "全部查不到时不应渲染任何卡片"
        );
    }

    /// 缩略图解码缓存的上限与回收语义：满了淘汰最久未用的那张、命中会提升、
    /// 同一张重新解码要把**旧图交还**给调用方（GPUI 靠 `drop_image` 释放纹理，
    /// 不交还就等于显存常驻）。
    #[test]
    fn thumb_image_lru_caps_and_returns_replaced_images() {
        let mut lru: ThumbImageLru<u32> = ThumbImageLru::new(2);
        assert!(lru.insert(1, 10).is_empty(), "未超上限：没有旧图需要回收");
        assert!(lru.insert(2, 20).is_empty());

        // 命中会把 1 提升为最近使用：下一个被淘汰的应是 2 而不是 1
        assert_eq!(lru.get(1), Some(&10));
        assert_eq!(lru.insert(3, 30), vec![20], "超上限淘汰最久未用的 2");
        assert!(lru.contains(1) && lru.contains(3) && !lru.contains(2));

        // 同一 id 重新解码：旧图必须交还给调用方显式回收
        assert_eq!(lru.insert(1, 11), vec![10], "替换下来的旧图要显式回收");
        assert_eq!(lru.get(1), Some(&11));
    }

    /// 记录被删除/库被刷新后，对应卡片的解码结果要能被清掉并交还回收。
    #[test]
    fn thumb_image_lru_prunes_missing_records() {
        let mut lru: ThumbImageLru<u32> = ThumbImageLru::new(4);
        lru.insert(1, 10);
        lru.insert(2, 20);
        lru.insert(3, 30);

        let live: std::collections::HashSet<i64> = [2_i64, 3].into_iter().collect();
        assert_eq!(lru.remove_missing(&live), vec![10], "只清掉已删除的记录");
        assert!(!lru.contains(1) && lru.contains(2) && lru.contains(3));
        // 全都在：没有任何东西需要回收
        assert!(lru.remove_missing(&live).is_empty());
    }

    /// 可视范围 → 解码派发区间：上下各留预取边距、两端夹进列表、空列表给空区间。
    #[test]
    fn thumb_demand_covers_viewport_with_margin() {
        assert_eq!(
            thumb_demand_slice(0, 0, 0, 4),
            0..0,
            "空列表不该派发任何解码"
        );
        // 视口在列表开头：上边距夹到 0
        assert_eq!(thumb_demand_slice(50, 0, 7, 4), 0..12);
        // 视口在中段：上下各扩 4 张
        assert_eq!(thumb_demand_slice(50, 20, 27, 4), 16..32);
        // 视口在结尾：下边距夹到 len
        assert_eq!(thumb_demand_slice(50, 42, 49, 4), 38..50);
        // 退化输入（首帧滚动句柄什么都没报）：至少仍覆盖一张，且不越界
        assert_eq!(thumb_demand_slice(50, 0, 0, 0), 0..1);
        // 越界下标先夹到末项、再按边距向两侧扩展（这里正好盖满整个 5 条列表）
        assert_eq!(thumb_demand_slice(5, 99, 99, 4), 0..5);
    }
}
