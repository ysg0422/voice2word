//! 翻译合并域：增量合并译文、文档代次与译文语言包切换。

use crate::subtitle::Segment;

use super::{source_language_label, AppState, ProcessStatus, TranslationBaseline};

impl AppState {
    /// 翻译失败的统一收口：写翻译面板文案，**仅在没有转写在跑时**才写全局 `status`。
    ///
    /// # 为什么不能无条件写全局 `status`
    ///
    /// `status` 只有一个格子。转写通常要跑几分钟，而这段时间里用户完全可以另起一个
    /// 翻译（翻译是纯后处理，入口只挡 `is_translating` / 空字幕表，不挡转写）。翻译失败
    /// 若在这里无条件 `status = Failed`，用户会看到「翻译失败」横幅、而状态栏里的转写
    /// 进度**凭空消失**——转写其实还在跑，界面却没有任何地方显示它。反过来，只在
    /// `translate_status_msg` 里记一句又会让「剪辑台上点翻译失败」这种没有翻译面板的
    /// 场景静默无声，所以按「有没有转写在跑」分流：没有转写才占用全局横幅。
    pub fn note_translate_failure(&mut self, msg: String) {
        self.translate_progress = 0.0;
        self.translate_status_msg = msg.clone();
        if self.is_processing() {
            // 转写在跑：只记日志 + 翻译面板，绝不碰转写进度态
            tracing::warn!("转写进行中，翻译失败只记录在翻译面板，不覆盖转写进度态: {msg}");
        } else {
            self.status = ProcessStatus::Failed(msg);
        }
    }
    /// 把翻译结果**按句合并**回当前字幕表，而不是整表覆盖。
    ///
    /// # 为什么不能直接 `self.segments = translated`
    ///
    /// 翻译是在后台跑几分钟的任务，而字幕编辑器在翻译期间**仍然可编辑**（用户
    /// 可以一边等一边改错别字、调时间、拆合句）。引擎拿到的是**开始时**克隆的一份
    /// 字幕快照，若收尾时直接 `self.segments = translated` 覆盖，就等于把用户在这
    /// 几分钟里做的所有编辑**静默回滚**——改了半天，翻译一结束全没了。
    ///
    /// # 为什么键是「起始时间」而不是 `index`
    ///
    /// 拆句 / 合句 / 删句都会调用 [`AppState::reindex_segments`] 把序号整体重排。
    /// 若按 `index` 合并，重排后的句子会拿到**别人**的译文——静默张冠李戴，比丢译文
    /// 严重得多（用户看到的是「译文内容对不上原句」却毫无提示）。
    ///
    /// 起始时间在三种结构编辑下都稳定：拆句时前半段保留原 `start`、合句时保留首段
    /// `start`、删句时其余句子不动。只有用户手动微调时间才会变，那种情况下宁可**漏认**
    /// （不合并，留待下次补译），也绝不错误合并。
    ///
    /// 只搬**非空**译文，不碰用户可能已改动的原文 / 时间 / 说话人。
    pub fn merge_translations(&mut self, translated: Vec<Segment>) {
        self.merge_translations_checked(translated, None);
    }

    /// 收尾合并（带冲突判定）：`baseline` 是开工时抓的句子基线。
    ///
    /// 判定规则（任一命中就**丢弃**这一条的机翻结果，保留用户当前的内容）：
    /// 1. **原文被改过**：开工时那句是 A、现在是 B，机翻时看到的是 A，套到 B 上就是
    ///    张冠李戴（这是唯一会静默毁掉用户手工劳动的路径）；
    /// 2. **译文被用户手改过**：句中译文与开工时不同，说明翻译这几分钟里用户自己
    ///    改/写了一条，机翻不该覆盖它。
    ///
    /// 返回被跳过的条数，供上层提示「N 句因期间编辑过而保留原样」。
    pub fn merge_translations_checked(
        &mut self,
        translated: Vec<Segment>,
        baseline: Option<&TranslationBaseline>,
    ) -> usize {
        use std::collections::HashMap;
        // 快照中的时间是原值复制，不能毫秒取整：相邻起点可能落到同一个毫秒。
        // 只归一化正负零；非有限时间没有可用的身份，不参与回填。
        let key = |start: f64| -> Option<u64> {
            start
                .is_finite()
                .then(|| if start == 0.0 { 0 } else { start.to_bits() })
        };
        let mut by_start: HashMap<u64, Vec<Segment>> = HashMap::with_capacity(translated.len());
        for seg in translated {
            if seg.has_translation() {
                if let Some(start) = key(seg.start) {
                    by_start.entry(start).or_default().push(seg);
                }
            }
        }
        let mut current_counts = HashMap::new();
        for seg in &self.segments {
            if let Some(start) = key(seg.start) {
                *current_counts
                    .entry((start, seg.translate_source()))
                    .or_insert(0usize) += 1;
            }
        }
        let mut skipped = 0usize;
        for seg in self.segments.iter_mut() {
            let Some(start) = key(seg.start) else {
                continue;
            };
            let Some(candidates) = by_start.get(&start) else {
                continue;
            };
            // 锁定句：译文是用户确认过的定稿，重译不得覆盖。
            // 这条与下面的 baseline 保护互补——baseline 只在本次运行内有效，
            // `locked` 随工程落库，重启后依然生效。
            if seg.locked {
                skipped += 1;
                continue;
            }
            let source = seg.translate_source();
            // 同起点按开工原文消歧；原文相同的重复字幕无法可靠区分，保留原样。
            let mut matching = candidates.iter().filter(|src| {
                let expected = match baseline {
                    Some(base) => base.source.get(&(src.index as i64)).cloned(),
                    None => Some(src.translate_source()),
                };
                expected.as_deref() == Some(source.as_str()) && src.translate_source() == source
            });
            let Some(src) = matching.next() else {
                skipped += 1;
                continue;
            };
            if matching.next().is_some() || current_counts.get(&(start, source)) != Some(&1) {
                skipped += 1;
                continue;
            }
            if let Some(base) = baseline {
                // 基线属于后台快照，必须用快照序号，当前表可能已经删句并重排。
                let idx = src.index as i64;
                // ② 译文被用户手改过：不覆盖
                let now = seg.translation.clone().filter(|t| !t.trim().is_empty());
                let then = base.translation.get(&idx).cloned().flatten();
                let incoming = src.translation.clone().filter(|t| !t.trim().is_empty());
                // `now == incoming` 是本函数上一次写下的值（增量落盘会反复合并同一份
                // 快照），不能误判成「人手改过」而跳过，否则第二次合并就全部失效。
                if now != then && now != incoming {
                    skipped += 1;
                    continue;
                }
            }
            // 先进语言包，再投影成「当前展示」：换目标语言重译时，旧语言的
            // 那一份留在 `translations` 里，用户随时能在「字幕语言包」切回去。
            if let (Some(lang), Some(text)) =
                (src.translation_lang.as_deref(), src.translation.as_deref())
            {
                seg.set_translation_pack(lang, text);
            }
            seg.translation = src.translation.clone();
            seg.translation_lang = src.translation_lang.clone();
        }
        self.bump_segments_revision();
        skipped
    }

    /// 文档代次 +1：整份替换字幕表时调用（导入 / 转写完成 / 打开工程）。
    pub fn bump_doc_epoch(&mut self) {
        self.doc_epoch = self.doc_epoch.wrapping_add(1);
    }

    /// 切换目标语言并落盘。目标语言是长期偏好，必须持久化，
    /// 否则重启后又跳回默认语言，用户会以为设置没生效。
    pub fn set_translate_target_lang(&mut self, lang: &str) {
        self.translate_target_lang = lang.to_string();
        self.config.translate.target_lang = lang.to_string();
        self.save_translate_config();
    }

    /// 这卷字幕里已经存在的「语言包」清单：`(语言名, 句数)`，按句数降序。
    ///
    /// 只统计**非空**译文。老工程没有语言包字段（只在 `translation` 里存了一份），
    /// 这里把它当作「只有这一种语言的包」兼容进来，不必等用户重译一遍。
    pub fn translation_packs(&self) -> Vec<(String, usize)> {
        let mut counts: std::collections::BTreeMap<String, usize> =
            std::collections::BTreeMap::new();
        for seg in &self.segments {
            for (lang, text) in &seg.translations {
                if !text.trim().is_empty() {
                    *counts.entry(lang.clone()).or_insert(0) += 1;
                }
            }
            if let (Some(lang), Some(text)) =
                (seg.translation_lang.as_deref(), seg.translation.as_deref())
            {
                if !text.trim().is_empty() && !seg.translations.contains_key(lang) {
                    *counts.entry(lang.to_string()).or_insert(0) += 1;
                }
            }
        }
        let mut out: Vec<(String, usize)> = counts.into_iter().collect();
        out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        out
    }

    /// 当前正被展示的语言包（`None` = 只显示原文）。
    ///
    /// 取「句数最多的那个语言」：混排数据（半篇英文半篇日语）下没有唯一答案，
    /// 但面板上高亮哪个必须有个确定值，否则每次重绘都可能换一个。
    pub fn current_display_pack(&self) -> Option<String> {
        let mut counts: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
        for seg in &self.segments {
            if seg.has_translation() {
                if let Some(lang) = seg.translation_lang.as_deref() {
                    *counts.entry(lang).or_insert(0) += 1;
                }
            }
        }
        counts
            .into_iter()
            .max_by_key(|(_, n)| *n)
            .map(|(lang, _)| lang.to_string())
    }

    /// 切换「当前展示 / 导出」的语言包：`Some(lang)` 展示该语言，`None` 只显示原文。
    ///
    /// 只改 `translation` / `translation_lang`——预览、双语导出、翻译统计全认这两个
    /// 字段，所以切语言包不用改下游一行；语言包本身一份都不会丢。
    pub fn apply_translation_pack(&mut self, lang: Option<&str>) {
        for seg in self.segments.iter_mut() {
            match lang {
                Some(l) => {
                    // 这一句没有该语言的包：清掉展示，避免留下上一门语言的残余。
                    if !seg.show_translation_pack(l) {
                        seg.show_source_only();
                    }
                }
                None => seg.show_source_only(),
            }
        }
        self.bump_segments_revision();
        self.segments_dirty = true;
        self.defer_segments_flush();
    }

    /// 切换「选句翻译」模式：关闭 → 开启（清空上次勾选）；开启 → 关闭。
    ///
    /// 开启时**必须**清空上一次的勾选：句子号是按文档编的，沿用上一个工程的
    /// 勾选会静默译错别的句子。
    pub fn toggle_translate_pick(&mut self) {
        self.translate_pick = match self.translate_pick.take() {
            Some(_) => None,
            None => Some(std::collections::BTreeSet::new()),
        };
    }

    /// 选句模式是否开启。
    pub fn is_picking_translate(&self) -> bool {
        self.translate_pick.is_some()
    }

    /// 某一句是否已被勾选（选句模式关闭时恒为 `false`）。
    pub fn is_translate_picked(&self, index: i64) -> bool {
        self.translate_pick
            .as_ref()
            .is_some_and(|set| set.contains(&index))
    }

    /// 勾选 / 取消一句；未处于选句模式时无副作用。
    pub fn toggle_translate_pick_one(&mut self, index: i64) {
        if let Some(set) = self.translate_pick.as_mut() {
            if !set.remove(&index) {
                set.insert(index);
            }
        }
    }

    /// 已勾选的句数（选句模式关闭时为 0）。
    pub fn translate_pick_count(&self) -> usize {
        self.translate_pick.as_ref().map_or(0, |set| set.len())
    }

    /// 把目标语言改成「当前视频语言」。
    ///
    /// 用户明确要求：翻译语言下拉框的默认选中项是**当前视频的语言**，而不是
    /// `config.toml` 里上次残留的那个。否则日语视频配上残留的「日本語」目标，
    /// 点「开始翻译」等于日语译日语（引擎会判定为复制并整片失败）。
    pub fn adopt_source_language_for_translation(&mut self) {
        // 换文档必须退出选句模式：勾选的是**上一份文档**的句子号，留着它下次
        // 点「开始翻译」就会去译这份文档里同号的、完全不相干的句子。
        self.translate_pick = None;
        let Some(lang) = source_language_label(&self.segments) else {
            return;
        };
        if self.translate_target_lang != lang {
            self.set_translate_target_lang(lang);
        }
    }

    /// 目标语言是否恰好就是这个视频本身的语言（此时翻译没有意义）。
    pub fn translate_target_equals_source(&self) -> bool {
        source_language_label(&self.segments).is_some_and(|lang| lang == self.translate_target_lang)
    }
}
