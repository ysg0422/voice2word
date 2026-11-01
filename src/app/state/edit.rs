//! 字幕编辑域：撤销/重做快照、拆分合并、时间微调、查找替换与错别字记忆。

use crate::subtitle::{plan_time_edit, Segment, MIN_EDIT_DUR};

use super::{AppState, EditSnapshot};

/// 合并两句原文时的拼接规则：西文之间补一个空格，中日韩/标点直接相连。
fn join_segment_text(left: &str, right: &str) -> String {
    if needs_join_space(left, right) {
        format!("{left} {right}")
    } else {
        format!("{left}{right}")
    }
}

/// 左串末尾与右串开头都是西文字母/数字时，连接处需要空格。
fn needs_join_space(left: &str, right: &str) -> bool {
    left.chars()
        .last()
        .is_some_and(|c| c.is_ascii_alphanumeric())
        && right
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
}

impl AppState {
    /// 错别字记忆库文件（放在应用根，随备份一起走）。
    fn typo_memory_path() -> std::path::PathBuf {
        crate::utils::AppConfig::app_root_dir().join("typo_memory.json")
    }

    /// 把一次手工订正（before → after）沉淀进错别字记忆库并落盘。
    ///
    /// 同一个人名/术语被 ASR 反复写错，是本地化场景最烦的事；用户每改一次就学一次，
    /// 下次转写/导入自动纠正。只在差异片段干净（非纯标点/数字）时才会被采纳，
    /// 判定在 `TypoMemory::learn_from_edit` 里。
    pub fn learn_typo_from_edit(&mut self, before: &str, after: &str) {
        if before == after {
            return;
        }
        let path = Self::typo_memory_path();
        let mut memory = crate::utils::typo_memory::TypoMemory::load(&path);
        if memory.learn_from_edit(before, after) > 0 {
            if let Err(err) = memory.save(&path) {
                tracing::warn!(error = %err, "错别字记忆库保存失败");
            }
        }
    }

    /// 把记忆库里学会的替换应用到全部字幕（文档装载时调用），返回改动句数。
    pub fn apply_typo_memory(&mut self) -> usize {
        let memory = crate::utils::typo_memory::TypoMemory::load(&Self::typo_memory_path());
        self.apply_typo_memory_with(&memory)
    }

    /// [`Self::apply_typo_memory`] 的可注入版本（单测用）。
    ///
    /// 与 `import_glossary_file_to` 同一套路：单测不能去读写仓库根的真实记忆库，
    /// 那会让同进程的其它测试一起看到污染数据。
    pub(super) fn apply_typo_memory_with(
        &mut self,
        memory: &crate::utils::typo_memory::TypoMemory,
    ) -> usize {
        if memory.is_empty() {
            return 0;
        }
        let mut changed = 0usize;
        for seg in self.segments.iter_mut() {
            // 锁定（已校对）的句子一律不动：这个函数在**装载文档时**自动跑，
            // 用户看不到它改了什么。把定稿句子再交给记忆库改写，正是那种
            // 「我明明改好保存了，第二天打开又变回去」的诡异现象。
            if seg.locked {
                continue;
            }
            let before_source = seg.display_text().to_string();
            let before_text = seg.text.clone();
            let before_polished = seg.polished.clone();
            if let Some(fixed) = memory.apply(&seg.text) {
                if fixed != seg.text {
                    seg.text = fixed;
                }
            }
            if !seg.polished.is_empty() {
                if let Some(fixed) = memory.apply(&seg.polished) {
                    seg.polished = fixed;
                }
            }
            if seg.display_text() != before_source {
                seg.invalidate_translations();
                seg.words.clear();
            }
            if seg.text != before_text || seg.polished != before_polished {
                changed += 1;
            }
        }
        if changed > 0 {
            self.bump_segments_revision();
            self.segments_dirty = true;
            self.defer_segments_flush();
        }
        changed
    }

    /// 清掉未锁定句子的指定语言译文（语言包 + 当前展示），支持撤销，用于「重译全部」。
    ///
    /// 引擎的增量语义是「`!translation_matches(target)` 才重译」，所以只要译文带对
    /// 语言标记就永远跳过——换了引擎/模型想重新出一遍时，此前没有任何入口。
    /// 返回被清理的句数。
    pub fn clear_translations_for_lang(&mut self, lang: &str) -> usize {
        if !self.segments.iter().any(|seg| {
            !seg.locked
                && (seg.translations.contains_key(lang)
                    || seg.translation_lang.as_deref() == Some(lang)
                    || seg.stale_translation_langs.contains(lang))
        }) {
            return 0;
        }
        self.snapshot_for_undo();
        let mut touched = 0usize;
        for seg in self.segments.iter_mut() {
            if seg.locked {
                continue;
            }
            let mut hit = seg.translations.remove(lang).is_some();
            hit |= seg.stale_translation_langs.remove(lang);
            if seg.translation_lang.as_deref() == Some(lang) {
                seg.translation = None;
                seg.translation_lang = None;
                hit = true;
            }
            if hit {
                touched += 1;
            }
        }
        if touched > 0 {
            self.bump_segments_revision();
            self.segments_dirty = true;
            self.defer_segments_flush();
        }
        touched
    }

    /// 获取当前播放时间对应的有效字幕片段
    /// 半开区间 [start, end)：边界时刻 (t == end == 下一句 start) 归下一句，
    /// 避免选中某句时画面字幕仍显示上一句
    pub fn get_active_segment(&self) -> Option<&Segment> {
        let t = self.current_time;
        self.segments
            .iter()
            .find(|seg| t >= seg.start && t < seg.end)
    }

    /// 选中指定索引的字幕片段
    pub fn select_segment(&mut self, index: usize) {
        self.selection_anchor = Some(index);
        self.set_active_segment(index);
    }

    /// 带修饰键的选中：`extend = true`（Shift 连点）时从锚点扩展到 `index`。
    ///
    /// 锚点缺失时退回「从当前选中项开始」，再退回 `index` 本身——
    /// 这样从菜单 / 快捷键进来的选中也不会把范围算歪。
    pub fn select_segment_with(&mut self, index: usize, extend: bool) {
        if extend {
            let anchor = self
                .selection_anchor
                .or(self.selected_segment_index)
                .unwrap_or(index);
            self.selection_anchor = Some(anchor);
            self.set_active_segment(index);
        } else {
            self.select_segment(index);
        }
    }

    /// 当前选中的句子区间（首尾都**含**），按 `index` 升序。未选中时为 `None`。
    pub fn selected_span(&self) -> Option<(usize, usize)> {
        let active = self.selected_segment_index?;
        let anchor = self.selection_anchor.unwrap_or(active);
        Some((active.min(anchor), active.max(anchor)))
    }

    /// 只改「当前句 + 编辑缓冲 + 播放头」，不动范围锚点。
    fn set_active_segment(&mut self, index: usize) {
        // 切句前把上一句的编辑登记为「稍后落库」：逐句跳转时每跳一次都序列化整份工程
        // 太贵（5000 句约 10~30ms），写库交给停手 1.2s 后的定时器
        // （见 `SEGMENTS_FLUSH_IDLE`）。关窗 / 换工程仍会立刻写，不会丢。
        self.defer_segments_flush();
        // 换了一句就是另一段编辑会话，之前那句的文本合并到此为止
        self.undo_text_coalesce = None;
        self.selected_segment_index = Some(index);
        if let Some(seg) = self.segments.iter().find(|s| s.index == index) {
            self.editing_text = seg.display_text().to_string();
            self.current_time = seg.start;
        }
    }

    /// 让「选中句 / 编辑缓冲 / 画面字幕」按**同一个判据**对齐到 `current_time`。
    ///
    /// 返回选中句是否发生了变化。
    ///
    /// # 为什么必须有这个唯一入口
    ///
    /// 监视器画面上压的字幕走 [`Self::get_active_segment`]（按 `current_time` 现算，
    /// 半开区间 `[start, end)`）；而右侧编辑卡里的正文走 `editing_text`——它是
    /// 「上一次选中某句时缓存下来的副本」。两者是**两个独立状态**：
    /// - 播放时钟每 40ms 推进 `current_time`，原先只把 `selected_segment_index`
    ///   改成命中的新句，**没有**同步 `editing_text`；
    /// - 于是一旦播到第 N 句，画面上已经是第 N 句的字幕，右侧编辑框却还停在第 N-2 句，
    ///   看起来就是「视频字幕和右边字幕对不上」。
    ///
    /// 本函数把这处归一：一律用 `get_active_segment()` 这一条判据定位命中句，
    /// 命中且与当前选中不同时，同步 `selected_segment_index` 与 `editing_text`，
    /// 并结束上一段「连续文本编辑合并」会话（与 `select_segment` 口径一致）。
    ///
    /// `preserve_editing`：用户此刻正在编辑框里打字时为 `true`——只跟随选中项、
    /// **不覆盖** `editing_text`，否则播放推进会把用户敲到一半的字冲掉。失焦后由
    /// 下一次选中自然对齐。跳转 / 外部驱动时间一律传 `false`。
    ///
    /// 注意：只改「视图游标」，不写文档内容——不 flush、不记撤销，才能在 40ms
    /// 的播放回调里每帧零成本调用。
    pub fn sync_selection_to_time(&mut self, preserve_editing: bool) -> bool {
        let Some(seg) = self.get_active_segment() else {
            return false;
        };
        let seg_idx = seg.index;
        if self.selected_segment_index == Some(seg_idx) {
            return false;
        }
        let text = seg.display_text().to_string();
        self.selected_segment_index = Some(seg_idx);
        if !preserve_editing {
            self.editing_text = text;
            self.undo_text_coalesce = None;
        }
        true
    }

    /// 跳转播放指针时间
    pub fn seek_to(&mut self, time_sec: f64) {
        // 拖动时间轴时本函数每 40ms 就被调一次（`seek_by_mouse_x` 的节流），
        // 这里绝不能同步写库：登记完就走，写库留给停手后的定时器。
        self.defer_segments_flush();
        let max_dur = if self.total_duration > 0.0 {
            self.total_duration
        } else {
            crate::subtitle::timeline_end_or(&self.segments, 3600.0)
        };
        self.current_time = time_sec.clamp(0.0, max_dur);
        // 跳转后让「选中句 / 编辑缓冲」跟随落点，判据与画面字幕同源（见上）。
        self.sync_selection_to_time(false);
    }

    /// 保存当前选中字幕片段的修改文本（仅内存，去抖后统一落库）
    pub fn save_selected_text(&mut self) {
        let Some(idx) = self.selected_segment_index else {
            return;
        };
        let new_text = self.editing_text.trim().to_string();
        // 文本编辑是逐键触发的，若每敲一个字都压一层撤销栈，用户按一次 Ctrl+Z
        // 只会退掉一个字符，栈也会被一次输入撑满。所以同一句的连续输入合并成
        // 一层：只有「换了一句」或「中间夹了别的编辑」时才另起一层。
        self.snapshot_for_text_edit(idx);
        // 审计也逐键追加（日志层自带「连续同句同类型合并」的 `coalesced`，
        // 展示时再折叠——这里若也做合并，就得把合并状态塞进 AppState，
        // 与日志的职责重叠）。
        let before_text = self
            .segments
            .iter()
            .find(|s| s.index == idx)
            .map(|s| s.display_text().to_string())
            .unwrap_or_default();
        self.audit(
            crate::subtitle::audit::EditKind::Text,
            idx,
            before_text,
            new_text.clone(),
        );
        // 改原文必须让所有旧语言包失效，保留文本与语言供参考和配音选声使用。
        if let Some(seg) = self.segments.iter_mut().find(|s| s.index == idx) {
            // 只有原文**真的变了**才动标记。本函数逐键触发，若「点进来又点出去」
            // 这种没改字的调用也清标记，用户会莫名其妙丢掉完成状态。
            // 用 `display_text()` 比较：界面与翻译实际取用的就是它（优先 `polished`），
            // 与下面赋值分支的口径一致（有 `polished` 时只改 `polished`）。
            let text_changed = seg.display_text() != new_text.as_str();
            seg.text = new_text.clone();
            if !seg.polished.is_empty() {
                seg.polished = new_text;
            }
            // 旧文本仍供参考，失效状态随语言包保存，切换语言不能恢复完成标记。
            if text_changed {
                seg.invalidate_translations();
                seg.words.clear();
            }
        }
        // 去抖：编辑逐键触发，若每次都全量序列化全部片段写 SQLite，长视频输入时是隐形 I/O 热点。
        // 仅标记脏，等切句/跳转/播放/导出等天然节点由 flush_segments_if_dirty 统一写回。
        self.segments_dirty = true;
        self.bump_segments_revision();
    }

    /// 直接改写当前选中片段的**译文**（用户在对照表里手工订正机翻）。
    ///
    /// 与原文编辑走同一条脏标记 + 去抖落库路径；译文本身不参与 `merge_translations`
    /// 的「只搬非空译文」覆盖，用户改完不会被下一次翻译任务回滚（那套合并按 start 匹配，
    /// 且只在引擎给出非空译文时才覆盖，见 `merge_translations`）。
    ///
    /// 传入空白串视为「清除译文」，回到无译文状态（对照表显示「—」）。
    pub fn set_selected_translation(&mut self, text: &str) {
        let Some(idx) = self.selected_segment_index else {
            return;
        };
        self.set_segment_translation(idx, text);
    }

    /// 指定句的译文编辑入口，供表格就地编辑与选句编辑共用。
    pub fn set_segment_translation(&mut self, idx: usize, text: &str) {
        if !self.segments.iter().any(|seg| seg.index == idx) {
            return;
        }
        self.snapshot_for_undo();
        let cleaned = text.trim().to_string();
        let before_translation = self
            .segments
            .iter()
            .find(|s| s.index == idx)
            .and_then(|s| s.translation.clone())
            .unwrap_or_default();
        self.audit(
            crate::subtitle::audit::EditKind::Translation,
            idx,
            before_translation,
            cleaned.clone(),
        );
        // P0-1：用户此刻是在「当前目标语言」下手工写这条译文，这个动作表达的就是
        // 「这条译文属于当前目标语言」，所以下面非空分支要**无条件**用当前目标语言
        // 覆盖标记。绝不能保留旧标记——那正是本缺陷的根因：English 译文 → 切到 日本語
        // → 手工改成日文，若标记仍是 "English"，`translation_matches("日本語")` 返回
        // false，这句被当**待译**，下一次翻译收尾就用机翻结果覆盖掉用户手写的日文；
        // 而全仓库没有任何 UI 写 `translation_lang`，用户没有「把这条标成日语」的入口，
        // 界面层无法自救，只能在这里落定。
        //
        // 取值来源与有效性：`translate_target_lang` 构造时取自
        // `config.translate.target_lang`（默认 "简体中文"，见
        // `default_translate_target_lang`），唯一写入方 `set_translate_target_lang`
        // 只接受 `TRANSLATE_TARGET_LANGS` 里的显示名，因此正常路径下必是非空显示名。
        // 防御：真出现空串时回落到配置值；两者都空则**不动标记**——写个空标记同样
        // 匹配不上任何语言，只会让这句反复被重译。
        let target_lang = if self.translate_target_lang.trim().is_empty() {
            self.config.translate.target_lang.clone()
        } else {
            self.translate_target_lang.clone()
        };
        if let Some(seg) = self.segments.iter_mut().find(|s| s.index == idx) {
            seg.cache_visible_translation();
            if cleaned.is_empty() {
                // 清除的是当前显示的那份；不让切换语言包时把旧译文恢复出来。
                let cleared_lang = seg.translation_lang.as_deref().unwrap_or(&target_lang);
                seg.translations.remove(cleared_lang);
                seg.stale_translation_langs.remove(cleared_lang);
                seg.translation = None;
                seg.translation_lang = None;
            } else {
                seg.translation = Some(cleaned);
                // 无条件覆盖标记（理由见函数开头的 P0-1 说明）。修前这里是
                // `if seg.translation_lang.is_none()`：只补「原先没有标记」的情况，
                // 于是「已有别的语言标记」的句子改完仍带着旧标记，下次翻译必然重译并覆盖。
                if !target_lang.trim().is_empty() {
                    seg.translation_lang = Some(target_lang.clone());
                }
                seg.cache_visible_translation();
                if !target_lang.trim().is_empty() {
                    seg.stale_translation_langs.remove(&target_lang);
                }
            }
        }
        self.segments_dirty = true;
        self.bump_segments_revision();
    }

    /// 撤销栈深度上限。按千行字幕、每段约 200 字节估，50 层也只有几 MB，
    /// 换来的是「一路点错也能退回去」，比为了省内存把深度压到个位数实用得多。
    const MAX_UNDO_DEPTH: usize = 50;

    fn current_snapshot(&self) -> EditSnapshot {
        EditSnapshot {
            segments: self.segments.clone(),
            selected_segment_index: self.selected_segment_index,
            selection_anchor: self.selection_anchor,
            current_time: self.current_time,
            editing_text: self.editing_text.clone(),
        }
    }

    /// 清空撤销/重做历史与文本合并标记。
    ///
    /// **凡是整体换掉 `segments`（换工程、缓存命中、清空工作区、新一轮转写覆盖）的地方都必须调用。**
    /// 撤销栈里装的是某个工程的片段快照，换文档后若不清理，用户按一次 Ctrl+Z 就会把
    /// 上一个工程的整份字幕灌回当前文档；`restore_snapshot` 收尾的 `flush_segments_if_dirty`
    /// 还会按当前工程的 id 写库，等于用旧工程覆盖掉新工程的历史记录。
    pub fn reset_edit_history(&mut self) {
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.undo_text_coalesce = None;
    }

    /// 记一条审计（内部便利方法，省掉调用点重复写 `DateTime::now()`）。
    ///
    /// 直接用 `Local::now()` 而不是让调用方传时间：审计要的是「真实发生时刻」，
    /// 而纯函数层（`audit.rs`）把时间作为参数收进来，正是为了**它可以被单测**。
    /// 两个目标不冲突：这里读钟、那边不读。
    fn audit(
        &mut self,
        kind: crate::subtitle::audit::EditKind,
        index: usize,
        before: String,
        after: String,
    ) {
        self.edit_log.push(crate::subtitle::audit::EditRecord {
            index,
            kind,
            before,
            after,
            at: chrono::Local::now(),
        });
    }

    /// 在执行一次编辑前记录快照。必须由**所有**编辑入口调用，
    /// 漏掉一处的后果是那类操作撤销时会连同前一次编辑一起退掉。
    pub fn snapshot_for_undo(&mut self) {
        let snap = self.current_snapshot();
        self.undo_stack.push(snap);
        if self.undo_stack.len() > Self::MAX_UNDO_DEPTH {
            self.undo_stack.remove(0);
        }
        // 非文本编辑打断文本合并：否则「改字 → 拆分 → 继续改字」会被并成一层，
        // 一次 Ctrl+Z 把拆分和两次改字一起退掉。
        self.undo_text_coalesce = None;
        // 新的编辑分支一旦产生，原先的重做链就失效了（与所有编辑器的行为一致）
        self.redo_stack.clear();
    }

    /// 文本编辑专用的快照入口：同一句的连续输入复用同一层撤销记录。
    fn snapshot_for_text_edit(&mut self, idx: usize) {
        if self.undo_text_coalesce == Some(idx) {
            return;
        }
        self.snapshot_for_undo();
        self.undo_text_coalesce = Some(idx);
    }

    /// 是否有可撤销的编辑（UI 据此决定撤销按钮是否可点）
    pub fn can_undo(&self) -> bool {
        !self.undo_stack.is_empty()
    }

    /// 是否有可重做的编辑
    pub fn can_redo(&self) -> bool {
        !self.redo_stack.is_empty()
    }

    /// 还原一份快照，并把界面依赖的状态一并刷新。
    ///
    /// 还原后必须递增 `segments_revision`：清单的搜索下标是按这个号缓存的，
    /// 不递增的话字幕表虽然换了，列表还会继续用旧下标渲染（表现为空白或错行）。
    fn restore_snapshot(&mut self, snap: EditSnapshot) {
        self.segments = snap.segments;
        self.selected_segment_index = snap.selected_segment_index;
        self.selection_anchor = snap.selection_anchor;
        self.current_time = snap.current_time;
        self.editing_text = snap.editing_text;
        self.segments_dirty = true;
        // 还原之后若还留着「正在合并的文本编辑」标记，用户接着打字会被并进
        // 已经回退掉的那一层，撤销栈顺序就乱了。
        self.undo_text_coalesce = None;
        self.bump_segments_revision();
        self.flush_segments_if_dirty();
    }

    /// 撤销最近一次字幕编辑，成功返回 `true`
    pub fn undo(&mut self) -> bool {
        let Some(snap) = self.undo_stack.pop() else {
            return false;
        };
        let current = self.current_snapshot();
        self.redo_stack.push(current);
        if self.redo_stack.len() > Self::MAX_UNDO_DEPTH {
            self.redo_stack.remove(0);
        }
        self.restore_snapshot(snap);
        true
    }

    /// 重做最近一次被撤销的编辑，成功返回 `true`
    pub fn redo(&mut self) -> bool {
        let Some(snap) = self.redo_stack.pop() else {
            return false;
        };
        let current = self.current_snapshot();
        self.undo_stack.push(current);
        if self.undo_stack.len() > Self::MAX_UNDO_DEPTH {
            self.undo_stack.remove(0);
        }
        self.restore_snapshot(snap);
        true
    }

    /// 微调选中字幕片段的起止时间
    ///
    /// 单纯加减时间会制造两类坏数据：区间交叉（后一句起点跑到前一句里面），
    /// 以及被压到近乎零长度。前者让「按播放时间取句」取错行，后者更狠——
    /// 下次载入工程时 [`crate::subtitle::optimize_segments`] 会把时长过短、
    /// 且与后句几乎同时开始的片段当成幻读鬼影**直接删掉**，用户看到的就是
    /// 「点几下微调，字幕没了，怎么改都回不来」。所以这里统一交给
    /// [`plan_time_edit`] 规划，宁可连带动一下邻居的边界，也不留下非法区间。
    pub fn adjust_selected_times(&mut self, delta_start: f64, delta_end: f64) {
        let Some(idx) = self.selected_segment_index else {
            return;
        };
        let Some(pos) = self.segments.iter().position(|s| s.index == idx) else {
            return;
        };

        let cur = self.segments[pos].clone();
        let prev = pos
            .checked_sub(1)
            .map(|p| (self.segments[p].start, self.segments[p].end));
        let next = self.segments.get(pos + 1).map(|s| (s.start, s.end));
        let edit = plan_time_edit(cur.start, cur.end, prev, next, delta_start, delta_end);

        self.snapshot_for_undo();
        // 审计记的是**规划后**的实际结果，不是用户点的那个 ±0.1——微调会被邻居
        // 边界挡住（`plan_time_edit` 可能收短/后挪），日志里若写「用户点了 +0.1」
        // 就与实际发生的事不符，事后对照会误导。
        self.audit(
            crate::subtitle::audit::EditKind::Timing,
            idx,
            format!("{:.3}-{:.3}", cur.start, cur.end),
            format!("{:.3}-{:.3}", edit.start, edit.end),
        );

        self.segments[pos].start = edit.start;
        self.segments[pos].end = edit.end;
        // 邻居只有在被本次微调越界时才跟着让位，没越界时 `plan_time_edit`
        // 返回 `None`，这里也就不会平白改动别人的时间。
        if let Some(prev_end) = edit.prev_end {
            if let Some(p) = pos.checked_sub(1) {
                self.segments[p].end = prev_end;
            }
        }
        if let Some(next_start) = edit.next_start {
            if pos + 1 < self.segments.len() {
                self.segments[pos + 1].start = next_start;
            }
        }

        self.segments_dirty = true;
        self.bump_segments_revision();
        self.flush_segments_if_dirty();
    }

    /// 拆分当前选中的字幕片段为前后两段。
    ///
    /// `split_at` 为按字符计的切分位置（来自编辑框光标）；`None` 时取文本正中。
    /// 切分时间点按字数的比例落在原时间区间内，而不是永远取中点，
    /// 否则在长句末尾断句时后半句会被拉到明显错误的时间位置。
    pub fn split_selected_segment(&mut self, split_at: Option<usize>) {
        let Some(idx) = self.selected_segment_index else {
            return;
        };
        let Some(pos) = self.segments.iter().position(|s| s.index == idx) else {
            return;
        };
        let orig = self.segments[pos].clone();

        let cur_text = orig.display_text().to_string();
        let char_count = cur_text.chars().count();
        if char_count <= 1 {
            return;
        }
        let split_pos = split_at.unwrap_or(char_count / 2).clamp(1, char_count - 1);
        let ratio = split_pos as f64 / char_count as f64;
        // 时间切点跟着文字比例走，但必须保证拆出来的两半都够长：时长短于
        // [`MIN_EDIT_DUR`] 且与邻句起点相近的片段会在下次载入时被 `optimize_segments`
        // 当幻读鬼影删掉，用户看到的就是「拆完再打开，半句话凭空没了」。
        // 原文总跨度不足两倍下限时无法拆成两段可读字幕，直接放弃这次拆分。
        let span = orig.end - orig.start;
        if span < 2.0 * MIN_EDIT_DUR {
            return;
        }
        let raw_split = orig.start + span * ratio;
        let split_time = raw_split.clamp(orig.start + MIN_EDIT_DUR, orig.end - MIN_EDIT_DUR);
        let part1: String = cur_text.chars().take(split_pos).collect();
        let part2: String = cur_text.chars().skip(split_pos).collect();

        // P0-3：译文必须**按同一个 ratio 切开**，左右各拿一半，绝不能整条复制给两半
        // （修前是 `translation: orig.translation.clone()`）：复制会让双语导出出现两个
        // cue 显示同一条完整译文（原文却已正确切开），而且本函数收尾立刻 flush 落库，
        // 重复译文会被持久化。切分策略直接复用自动拆分的
        // `subtitle::segment::split_following`，两边同一份实现：先按标点就近切、
        // 找不到标点再按比例硬切，**绝不让后半段落空**。
        let (trans_left, trans_right) = match orig.translation.as_deref() {
            Some(t) if !t.trim().is_empty() => {
                let (l, r) = crate::subtitle::segment::split_following(t, ratio);
                (Some(l), r)
            }
            // 没有译文（或只有空白）时两半都不带译文：不能凭空造出 `Some("")`，
            // 那会让双语导出多出一行空白。
            _ => (None, None),
        };
        let trans_lang = if trans_left.is_some() {
            orig.translation_lang.clone()
        } else {
            None
        };

        // F11：语言包也要按同一 ratio 切开。修前左半保留**整张包**、右半写
        // `Default::default()`：于是左半挂着整句旧译文、右半没有译文，切语言包时
        // 同一句上下两半语言还不一致。实现收敛到
        // `subtitle::segment::split_translations_pack`，与自动拆句
        // （`split_segment_recursive`）共用一份：当前展示译文比语言包条目新时，
        // 切开的两半会覆盖同名条目，右半不再缺当前语言那份。
        let (left_packs, right_packs) = crate::subtitle::segment::split_translations_pack(
            &orig.translations,
            trans_lang.as_deref(),
            orig.translation.as_deref(),
            ratio,
        );
        // 词级时间戳按切点分到两半：左半不能再保留切点之后的词，否则卡拉OK
        // 会把整句词高亮画到已经被切走的文本上。
        let (words_left, words_right): (Vec<_>, Vec<_>) = orig
            .words
            .iter()
            .cloned()
            .partition(|w| w.start < split_time);

        self.snapshot_for_undo();
        self.segments[pos].end = split_time;
        if !self.segments[pos].polished.is_empty() {
            self.segments[pos].polished = part1;
        } else {
            self.segments[pos].text = part1;
        }
        // 左半段也要换成切分后的译文；原样保留（修前行为）等于把整条译文复制一份。
        self.segments[pos].translation = trans_left;
        self.segments[pos].translation_lang = trans_lang.clone();
        self.segments[pos].translations = left_packs;
        self.segments[pos].words = words_left;

        let new_seg = Segment {
            // 定稿锁随两半继承：手动拆句只是把定稿内容切成两条，不构成
            // 「用户撤销了定稿」。此前右半写死 `false`，锁定句拆开后右半会被
            // 「重译全部」覆盖掉用户校对过的译文。要放开编辑，用户可再点一次锁定按钮。
            locked: orig.locked,
            index: orig.index + 1,
            start: split_time,
            end: orig.end,
            text: part2.clone(),
            translation: trans_right,
            translation_lang: trans_lang,
            translations: right_packs,
            stale_translation_langs: orig.stale_translation_langs.clone(),
            polished: if !orig.polished.is_empty() {
                part2
            } else {
                String::new()
            },
            language: orig.language.clone(),
            confidence: orig.confidence,
            speaker: orig.speaker,
            speaker_name: orig.speaker_name.clone(),
            // 右半承接切点之后的词（之前一律留空，词级高亮会在拆分后整句降级）。
            words: words_right,
        };
        self.segments.insert(pos + 1, new_seg);

        // 重新规范化所有序号
        self.reindex_segments();
        self.bump_segments_revision();
        self.select_segment(idx + 1);
        self.segments_dirty = true;
        self.flush_segments_if_dirty();
    }

    /// 将当前选中字幕与下一段字幕合并
    pub fn merge_selected_with_next(&mut self) {
        let Some(idx) = self.selected_segment_index else {
            return;
        };
        let Some(pos) = self.segments.iter().position(|s| s.index == idx) else {
            return;
        };
        if pos + 1 >= self.segments.len() {
            return;
        }

        self.snapshot_for_undo();
        // 合并同样是破坏性的（少了一个 cue）。记下合并前的两句原文。
        let before_merge = format!(
            "{} | {}",
            self.segments[pos].display_text(),
            self.segments[pos + 1].display_text()
        );
        self.audit(
            crate::subtitle::audit::EditKind::Merge,
            idx,
            before_merge,
            String::new(),
        );
        let mut next = self.segments.remove(pos + 1);
        next.cache_visible_translation();
        self.segments[pos].cache_visible_translation();
        // P0-4：译文不能丢。`next` 被 remove 之后它的译文就没了，而原文是「拼接」的，
        // 合并后的 cue 理应对应两段译文的拼接。修前这里完全没碰译文：首句无译文、
        // 次句有译文时，合并后译文直接消失（双语导出少一行），且立刻落库。
        // 必须在 `cur` 被可变借用之前把两边的译文取出来。
        let (merged_translation, merged_translation_lang) = merge_concat_translations(
            self.segments[pos].translation.as_deref(),
            self.segments[pos].translation_lang.as_deref(),
            next.translation.as_deref(),
            next.translation_lang.as_deref(),
        );
        let left = &self.segments[pos];
        let incomplete_langs: std::collections::BTreeSet<_> = left
            .translations
            .keys()
            .chain(next.translations.keys())
            .filter(|lang| {
                (!left.translate_source().trim().is_empty()
                    && left.translation_pack(lang).is_none())
                    || (!next.translate_source().trim().is_empty()
                        && next.translation_pack(lang).is_none())
            })
            .cloned()
            .collect();
        let cur = &mut self.segments[pos];
        cur.stale_translation_langs.extend(incomplete_langs);
        cur.stale_translation_langs
            .extend(next.stale_translation_langs.iter().cloned());
        cur.end = cur.end.max(next.end);
        // F11：西文之间补一个空格，否则 "Hello"+"world" 会粘成 "Helloworld"。
        let joined_text = join_segment_text(cur.display_text(), next.display_text());
        if !cur.polished.is_empty() || !next.polished.is_empty() {
            cur.polished = joined_text;
        } else {
            cur.text = joined_text;
        }
        cur.translation = merged_translation;
        cur.translation_lang = merged_translation_lang;
        // 词级时间戳与语言包同样要并进来：以前 `next` 被 remove 后这两样直接消失。
        cur.words.extend(next.words.iter().cloned());
        for (lang, text) in next.translations.iter() {
            if text.trim().is_empty() {
                continue;
            }
            cur.translations
                .entry(lang.clone())
                .and_modify(|existing| {
                    if !existing.trim().is_empty() {
                        existing.push('\n');
                    }
                    existing.push_str(text);
                })
                .or_insert_with(|| text.clone());
        }
        // 当前语言可能只在另一句的语言包中有译文，投影完整合并结果，避免显示半句。
        if let Some(text) = cur
            .translation_lang
            .as_deref()
            .and_then(|lang| cur.translation_pack(lang))
        {
            cur.translation = Some(text.to_string());
        }

        self.reindex_segments();
        self.bump_segments_revision();
        self.select_segment(idx);
        self.segments_dirty = true;
        self.flush_segments_if_dirty();
    }

    /// 删除当前选中的字幕片段
    /// 用「局部重转」的结果整段替换 `[from, to]` 这些句子。
    ///
    /// 与单句版本的区别：多句重转时新结果的分句数**通常与原句数不同**（一句话被
    /// 切成两句、或两句被并成一句），逐句对位必然错位。所以这里整段换掉：删掉
    /// 原来的 N 句、插入新的 M 句，时间轴取自新结果并**钳在被替换区间内**——
    /// 区间外的句子时间一律不动。
    ///
    /// 返回是否发生了替换（无选中 / 新结果为空 / 区间越界都返回 `false`）。
    pub fn replace_segment_span(
        &mut self,
        from: usize,
        to: usize,
        new_segments: Vec<Segment>,
    ) -> bool {
        if new_segments.is_empty() {
            return false;
        }
        let (lo, hi) = (from.min(to), from.max(to));
        let positions: Vec<usize> = self
            .segments
            .iter()
            .enumerate()
            .filter(|(_, s)| s.index >= lo && s.index <= hi)
            .map(|(i, _)| i)
            .collect();
        let (Some(&first), Some(&last)) = (positions.first(), positions.last()) else {
            return false;
        };
        let (span_start, span_end) = (self.segments[first].start, self.segments[last].end);

        self.snapshot_for_undo();
        let before: String = positions
            .iter()
            .map(|i| self.segments[*i].display_text())
            .collect::<Vec<_>>()
            .join(" ");
        let after: String = new_segments
            .iter()
            .map(|s| s.display_text())
            .collect::<Vec<_>>()
            .join(" ");
        self.audit(crate::subtitle::audit::EditKind::Text, lo, before, after);

        let mut incoming = new_segments;
        for seg in incoming.iter_mut() {
            // 新结果的时间轴来自同一段音频，理论上已落在区间内；这里再钳一次，
            // 防止引擎给出越界时间戳把邻居的句子顶开。
            seg.start = seg.start.clamp(span_start, span_end);
            seg.end = seg.end.clamp(seg.start, span_end);
            // 重转出来的句子是「机器新写的」，不能继承原来的已校对标记。
            seg.locked = false;
        }
        self.segments.splice(first..=last, incoming);
        self.reindex_segments();
        self.segments_dirty = true;
        self.bump_segments_revision();
        // 选中新插入的第一句（原区间起点位置）。
        if let Some(seg) = self.segments.get(first) {
            let idx = seg.index;
            self.select_segment(idx);
        }
        true
    }

    /// 用「局部重转」的结果替换某一句的原文；返回是否真的改了。
    ///
    /// 只动 `text`（并清掉已失效的 `polished`），**不动**起止时间：时间轴是用户
    /// 按画面卡好的，重转只是换文字；改时间会把整条轨道的对齐关系打乱。
    pub fn replace_segment_text(&mut self, index: usize, text: String) -> bool {
        let Some(pos) = self.segments.iter().position(|s| s.index == index) else {
            return false;
        };
        let before = self.segments[pos].display_text().to_string();
        if before == text {
            return false;
        }
        self.snapshot_for_undo();
        self.audit(
            crate::subtitle::audit::EditKind::Text,
            index,
            before,
            text.clone(),
        );
        let seg = &mut self.segments[pos];
        seg.text = text;
        // 旧润色稿是按旧原文生成的，留着会让显示文本还是错的。
        seg.polished.clear();
        seg.invalidate_translations();
        seg.words.clear();
        self.segments_dirty = true;
        self.bump_segments_revision();
        true
    }

    /// 切换当前选中句的「已锁定」状态；返回切换后的值（未选中时为 `None`）。
    ///
    /// 锁定语义：这一句的译文/文本已经**人工定稿**，重新翻译与润色都不得覆盖。
    /// 与 `merge_translations_checked` 里的 baseline 保护互补——那个只在本次运行内
    /// 有效，本标记随工程落库。
    pub fn toggle_selected_lock(&mut self) -> Option<bool> {
        let idx = self.selected_segment_index?;
        let pos = self.segments.iter().position(|s| s.index == idx)?;
        let seg = self.segments.get_mut(pos)?;
        seg.locked = !seg.locked;
        let locked = seg.locked;
        self.segments_dirty = true;
        self.bump_segments_revision();
        Some(locked)
    }

    pub fn delete_selected_segment(&mut self) {
        let Some(idx) = self.selected_segment_index else {
            return;
        };
        let Some(pos) = self.segments.iter().position(|s| s.index == idx) else {
            return;
        };
        self.snapshot_for_undo();
        // 删除是破坏性操作：审计必须留下**被删掉的内容**，否则事后无从追查
        // 「原来那句是什么」（撤销栈只有 50 层，且换文档即清空）。
        let removed_text = self.segments[pos].display_text().to_string();
        self.audit(
            crate::subtitle::audit::EditKind::Delete,
            idx,
            removed_text,
            String::new(),
        );
        self.segments.remove(pos);

        self.reindex_segments();
        self.bump_segments_revision();
        if !self.segments.is_empty() {
            let next_idx = self.segments[pos.min(self.segments.len() - 1)].index;
            self.select_segment(next_idx);
        } else {
            self.selected_segment_index = None;
            self.editing_text.clear();
        }
        self.segments_dirty = true;
        self.flush_segments_if_dirty();
    }

    pub(super) fn reindex_segments(&mut self) {
        for (i, seg) in self.segments.iter_mut().enumerate() {
            seg.index = i + 1;
        }
    }

    /// 「修复常见错误」的预览：按当前勾选的规则能改多少处、分别是什么。
    ///
    /// 每帧现算而不是缓存：纯字符串扫描（一小时素材量级在毫秒以下），比再引一份
    /// 缓存与失效键便宜得多，也不会出现「预览是旧的、落地是新的」这种两张皮。
    pub fn cleanup_preview(&self) -> crate::subtitle::cleanup::CleanupPlan {
        crate::subtitle::cleanup::plan(&self.segments, self.cleanup_rules)
    }

    /// 勾选/取消一条修复规则。规则一变，上一条「已修复 N 处」的提示就成了旧结果，
    /// 继续挂着会把用户引到已经不成立的结论上，所以一起清掉。
    pub fn toggle_cleanup_rule(&mut self, rule: crate::subtitle::cleanup::CleanupRule) {
        self.cleanup_rules.toggle(rule);
        self.cleanup_note = None;
    }

    /// 一键修复常见错误（语气词 / 重复字词 / 听障标记 / 空白标点）。
    ///
    /// 没有可改之处时**不压撤销点**：空操作占掉一层 Ctrl+Z，用户按下去只会把上一次
    /// 真正的编辑退掉，看起来像「撤销坏了」。
    pub fn apply_cleanup(&mut self) -> crate::subtitle::cleanup::CleanupPlan {
        let plan = crate::subtitle::cleanup::plan(&self.segments, self.cleanup_rules);
        if plan.is_empty() {
            self.cleanup_note = Some(if self.cleanup_rules.any() {
                "按当前勾选的规则没有发现可修复的地方".to_string()
            } else {
                "先勾选至少一条规则".to_string()
            });
            return plan;
        }
        self.snapshot_for_undo();
        // 审计按规则记摘要，不逐句记：一次清洗动辄上百句，逐句写会把有界日志瞬间
        // 挤满，把它们之后真正需要追溯的人工编辑挤出去。
        for rule in crate::subtitle::cleanup::CleanupRule::ALL {
            let hits: Vec<&crate::subtitle::cleanup::CleanupHit> =
                plan.hits.iter().filter(|h| h.rule == rule).collect();
            let Some(first) = hits.first() else {
                continue;
            };
            let sample = format!(
                "{} → {}",
                audit_snippet(&first.before),
                audit_snippet(&first.after)
            );
            self.audit(
                crate::subtitle::audit::EditKind::Replace,
                first.index,
                format!("{} 处未修复（{}）", hits.len(), rule.label()),
                sample,
            );
        }
        crate::subtitle::cleanup::apply(&mut self.segments, self.cleanup_rules);
        self.segments_dirty = true;
        self.bump_segments_revision();
        self.flush_segments_if_dirty();
        let emptied = plan.emptied_count();
        self.cleanup_note = Some(if emptied > 0 {
            format!(
                "已修复 {} 处（{emptied} 句只剩标记或停顿词，已清空，要不要删条由你决定）",
                plan.len()
            )
        } else {
            format!("已修复 {} 处，可 Ctrl+Z 撤销", plan.len())
        });
        plan
    }

    /// 阅读速度超限的句下标（上限见 [`crate::subtitle::DEFAULT_MAX_CPS`]）。
    ///
    /// 薄封装 [`crate::subtitle::cps_violations`]：可见字符数与统计面板同一口径，
    /// 上限非法 / 非正时按字符集分档回落，零长与倒挂句也入列。只读 `segments`。
    pub fn cps_violation_indices(&self, max_cps: f64) -> Vec<usize> {
        crate::subtitle::cps_violations(&self.segments, max_cps)
    }

    /// 句尾标点缺失的复核计划（只报告，不改文本）。
    ///
    /// 薄封装 [`crate::subtitle::plan_punctuation_fix`]：补标点必须猜语气，工具不替用户
    /// 改正文，这里只产出「哪几句值得看一眼」。只读 `segments`。
    pub fn punctuation_plan(&self) -> crate::subtitle::PunctuationPlan {
        crate::subtitle::plan_punctuation_fix(&self.segments)
    }

    /// 按 [`crate::subtitle::plan_cps_fix`] 延长过短句的终点（只动时间、不动文本）。
    ///
    /// 写回口径与 [`Self::apply_cleanup`] 逐条一致：有改动时先
    /// [`Self::snapshot_for_undo`] 压快照，再改 `segments`、置脏、递增修订号、落库；
    /// **无改动不压撤销点**（空操作占掉一层 Ctrl+Z 会让用户觉得「撤销坏了」）。
    ///
    /// `plan_cps_fix` 保证 `plan.times` 与输入等长同序，因此这里按下标逐条写回即可；
    /// 上限 / 时长下限的默认值分别为 [`crate::subtitle::DEFAULT_MAX_CPS`] 与
    /// [`crate::subtitle::MIN_EDIT_DUR`]，界面传参不要硬编码这两个数字。
    pub fn apply_cps_fix(&mut self, max_cps: f64, min_dur: f64) -> crate::subtitle::CpsFixPlan {
        let plan = crate::subtitle::plan_cps_fix(&self.segments, max_cps, min_dur);
        if !plan.has_changes() {
            return plan;
        }
        self.snapshot_for_undo();
        for (seg, (start, end)) in self.segments.iter_mut().zip(plan.times.iter()) {
            seg.start = *start;
            seg.end = *end;
        }
        self.segments_dirty = true;
        self.bump_segments_revision();
        self.flush_segments_if_dirty();
        plan
    }

    /// 相邻重复句的预览（界面据此显示「N 组重复、可删 M 句」）。
    pub fn duplicate_cue_groups(&self) -> Vec<crate::subtitle::cleanup::DuplicateGroup> {
        crate::subtitle::cleanup::duplicate_groups(&self.segments)
    }

    /// 折叠相邻重复句：每组保留第一句并让它的结束时间盖住整组，其余删掉。
    ///
    /// 为什么交给用户点而不是自动做：真说过两遍同样的话（副歌、口头禅）与模型重复
    /// 出句在数据上长得一样，只差时间间隔；规则给出候选，删不删由人决定——所以
    /// 界面先显示组数与被删句数，让用户看清代价再点。
    pub fn fold_duplicate_segments(&mut self) -> usize {
        let groups = crate::subtitle::cleanup::duplicate_groups(&self.segments);
        if groups.is_empty() {
            self.cleanup_note = Some("没有发现相邻重复句".to_string());
            return 0;
        }
        // 折叠会让被删句之后的序号整体前移，所以先按「原表位置」记下两件事：选中句的
        // 位置，以及它被折进哪一组（选中句本身就是要被删掉的那句时）。不这么做，
        // 用户会发现「点了一下折叠，选中的句子悄悄换成了另一句」。
        let selected_index = self.selected_segment_index;
        let selected_pos =
            selected_index.and_then(|idx| self.segments.iter().position(|s| s.index == idx));
        let folded_owner_pos: Option<usize> = selected_index.and_then(|idx| {
            groups
                .iter()
                .find(|g| g.indices.iter().skip(1).any(|i| *i == idx))
                .and_then(|g| g.indices.first().copied())
                .and_then(|keep| self.segments.iter().position(|s| s.index == keep))
        });
        let removed: Vec<usize> = groups
            .iter()
            .flat_map(|g| g.indices.iter().skip(1).copied())
            .collect();
        let survivors: Vec<usize> = self
            .segments
            .iter()
            .enumerate()
            .filter(|(_, s)| !removed.contains(&s.index))
            .map(|(pos, _)| pos)
            .collect();

        self.snapshot_for_undo();
        for group in &groups {
            let index = group.indices.first().copied().unwrap_or(0);
            self.audit(
                crate::subtitle::audit::EditKind::Merge,
                index,
                format!(
                    "{} 句重复：{}",
                    group.indices.len(),
                    audit_snippet(&group.text)
                ),
                format!("保留 1 句，时间 {:.2}-{:.2}", group.start, group.end),
            );
        }
        let report = crate::subtitle::cleanup::fold_duplicates(&mut self.segments);
        // 删条会改序号与清单长度：与 `delete_selected_segment` 同一套收尾。
        self.reindex_segments();
        if selected_index.is_some() {
            // 选中句还在就跟随同一句（删除发生在它之前时序号前移），被折掉就跟随
            // 它并进去的那一句——那正是用户原本在看的那句话的归并结果。
            let anchor = folded_owner_pos.or(selected_pos);
            let new_pos = anchor.and_then(|pos| survivors.iter().position(|&s| s == pos));
            match new_pos.and_then(|pos| self.segments.get(pos).map(|s| s.index)) {
                Some(idx) => self.select_segment(idx),
                None => match self.segments.first().map(|s| s.index) {
                    Some(first) => self.select_segment(first),
                    None => {
                        self.selected_segment_index = None;
                        self.editing_text.clear();
                    }
                },
            }
        }
        self.segments_dirty = true;
        self.bump_segments_revision();
        self.flush_segments_if_dirty();
        self.cleanup_note = Some(format!(
            "已折叠 {} 组重复句（删除 {} 句），可 Ctrl+Z 撤销",
            report.groups, report.removed
        ));
        report.removed
    }
}

fn audit_snippet(text: &str) -> String {
    const MAX_CHARS: usize = 40;
    let flat = text.replace(['\r', '\n'], " ");
    let flat = flat.trim();
    let mut out: String = flat.chars().take(MAX_CHARS).collect();
    if flat.chars().count() > MAX_CHARS {
        out.push('…');
    }
    out
}

fn merge_concat_translations(
    cur: Option<&str>,
    cur_lang: Option<&str>,
    next: Option<&str>,
    next_lang: Option<&str>,
) -> (Option<String>, Option<String>) {
    let cur_text = cur.filter(|t| !t.trim().is_empty());
    let next_text = next.filter(|t| !t.trim().is_empty());
    let cur_lang = cur_lang.filter(|l| !l.trim().is_empty());
    let next_lang = next_lang.filter(|l| !l.trim().is_empty());
    match (cur_text, next_text) {
        (None, None) => (None, None),
        (Some(t), None) => (Some(t.to_string()), cur_lang.map(str::to_string)),
        (None, Some(t)) => (Some(t.to_string()), next_lang.map(str::to_string)),
        (Some(a), Some(b)) => {
            let lang = match (cur_lang, next_lang) {
                (Some(l), Some(r)) if l == r => Some(l.to_string()),
                _ => None,
            };
            (Some(format!("{a}\n{b}")), lang)
        }
    }
}
