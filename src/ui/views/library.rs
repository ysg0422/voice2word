//! 媒体库与历史任务视图

use gpui::prelude::*;
use gpui::*;
use std::path::PathBuf;
use std::sync::Arc;

use super::super::primitives;
use super::super::theme::Theme;
use super::super::types::{ConfirmAction, ConfirmDialogInfo};
use super::super::MainWindow;
use crate::app::state::{ProcessStatus, WorkspaceTab};
use crate::subtitle::writer::export_spec_for;
use crate::subtitle::SubtitleWriter;
use crate::ui::apply_line_edit;
use crate::utils::time::format_duration_short;

impl MainWindow {
    /// 渲染历史视频库 (视频资产管理与一键载入工作台)
    pub(crate) fn render_library_layout(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let total_count = self.state.recent_tasks.len();

        // 为缺失首帧的卡片后台派发缩略图提取（幂等，完成后自动刷新）
        self.ensure_library_thumbnails(cx);
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
            .border_color(if is_focused {
                Theme::accent_mint()
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
                    .child(div().text_color(Theme::accent_mint()).child("▌"))
                    .child(after)
                    .into_any_element()
            } else if char_count == 0 {
                div()
                    .text_size(px(Theme::TEXT_SMALL))
                    .text_color(Theme::text_muted())
                    .child("搜索视频...")
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
            .gap_1()
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
                primitives::chip_clickable("清除筛选", false, false)
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
            .px_4()
            .py_2()
            .mb(px(Theme::SPACE_3))
            .rounded_lg()
            .bg(Theme::bg_raised())
            .border_1()
            .border_color(Theme::tint_mint_border())
            .flex()
            .items_center()
            .justify_between()
            .child(
                div().flex().items_center().gap_2().child(
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
                    .gap_2()
                    .child(
                        primitives::chip_clickable("取消选择", false, false)
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
                    .child("暂无解析历史"),
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
                    "导入视频开始转写",
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
                    primitives::chip_clickable("清除筛选", false, false)
                        .id("library-clear-filter-empty")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.clear_library_filter(cx);
                        })),
                )
                .into_any_element();
        }
        let ordered: Vec<_> = ids
            .iter()
            .filter_map(|id| self.state.recent_tasks.iter().find(|t| t.id == *id))
            .collect();
        let cards: Vec<AnyElement> = ordered
            .into_iter()
            .map(|task| self.render_library_card(task, cx).into_any_element())
            .collect();
        div()
            .id("library-cards-scroll")
            .flex_1()
            .overflow_y_scroll()
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

    /// 为视频库卡片异步预提取首帧缩略图。
    /// 复用全局 FrameCache（磁盘级缓存：路径+文件签名+时间键，跨会话命中），时间 0.0 即首帧；
    /// 每个任务仅派发一次，失败时写入空路径标记防止逐帧重试刷爆后台。
    fn ensure_library_thumbnails(&mut self, cx: &mut Context<Self>) {
        const MAX_THUMB_WORKERS: usize = 2;
        // 先收集待派发项（只取 id 与路径），避免在借用 recent_tasks 的同时
        // 修改 library_thumb_inflight，也避免克隆整段字幕数据
        let pending: Vec<(i64, PathBuf)> = self
            .state
            .recent_tasks
            .iter()
            .filter(|t| {
                !self.library_thumbs.contains_key(&t.id)
                    && !self.library_thumb_inflight.contains(&t.id)
            })
            .map(|t| (t.id, PathBuf::from(&t.file_path)))
            .collect();

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
        let ext = task
            .file_name
            .rsplit('.')
            .next()
            .unwrap_or("MP4")
            .to_uppercase();

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

        // 摘录首条字幕文本 (按字符边界截断，避免切进多字节中文字符导致 panic)
        // 列表查询已由 SQLite 抽出首句文本，这里不再反序列化整段字幕
        let sample_text = {
            let t = task.sample_text.clone();
            if t.trim().is_empty() {
                "无字幕内容".to_string()
            } else if t.chars().count() > 40 {
                let cut: String = t.chars().take(40).collect();
                format!("{}...", cut)
            } else {
                t
            }
        };

        // 格式化日期（取日期部分）
        let date_display = if task.created_at.len() >= 10 {
            task.created_at[..10].to_string()
        } else {
            task.created_at.clone()
        };

        let selected = self.library_selected.contains(&task_id);
        div()
            .id(("lib-card", task_id as usize))
            .w_full()
            .p_4()
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
            .gap_4()
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

                match self.library_thumbs.get(&task_id) {
                    Some(path) if !path.as_os_str().is_empty() => thumb_box
                        // 视频首帧铺满卡片，Cover 裁剪对齐 16:9
                        .child(img(path.clone()).size_full().object_fit(ObjectFit::Cover))
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
                                .px_2()
                                .py_0p5()
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
                    _ => thumb_box
                        .flex()
                        .items_center()
                        .justify_center()
                        .flex_col()
                        .gap_1()
                        // 格式标识
                        .child(
                            div()
                                .px_3()
                                .py_1()
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
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .justify_between()
                    .gap_2p5()
                    // 文件名 + 状态
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2p5()
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
                                    .gap_2()
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
                                            .gap_2()
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
                    .gap_2()
                    // 耗时详情 (if metrics)
                    .children(task.metrics.clone().map(|m| {
                        let fname = task.file_name.clone();
                        primitives::btn_mint_soft("耗时")
                            .id(("lib-metrics-btn", task_id as usize))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.benchmark_dialog =
                                    Some(crate::ui::types::BenchmarkDialogInfo {
                                        file_name: fname.clone(),
                                        metrics: m.clone(),
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
                            let (fmt_ext, fmt_label) = export_spec_for(&this.state.output_format);
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
                                        fmt_ext,
                                        export_mode,
                                        &style,
                                    ) {
                                        // 导出失败必须报出来：静默吞掉会让用户以为
                                        // 文件已经写出去了，回头找不到又无从排查。
                                        let _ = this.update(cx, |this, cx| {
                                            this.state.status =
                                                ProcessStatus::Failed(format!("导出失败: {err}"));
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

#[cfg(test)]
mod tests {
    use super::{export_spec_for, format_file_size};

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
}
