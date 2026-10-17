//! 错别字记忆库：把用户**手工改过的错字**沉淀成一张「错 → 对」替换表。
//!
//! # 为什么需要它
//!
//! 语音识别引擎对同一段素材、同一个专有名词会**稳定地**错成同一个样子：`机器学习`
//! 被识成 `机哭学习`、`OpenAI` 被识成 `Open AI`、人名地名同音字写错。用户每转写一
//! 期节目都得手动改一遍同一批错字；下次换一个文件，错误原样回来。本模块把「用户刚
//! 才的这次修改」拆成一个替换对存下来，之后的转写 / 导入直接把已知错误改掉。
//!
//! # 边界
//!
//! - **纯逻辑 + 一次文件读写**：不读配置、不碰 UI、不认识 `EditRecord`。把审计记录
//!   翻译成文本对是 `subtitle::audit::typo_learning_pair` 的事（依赖方向只能是
//!   `subtitle → utils`，反过来会让本模块为了一个文件名而拖进整条字幕链）。
//! - **只学「等长上下文里被替换的那一段」**：用公共前后缀定位差异区间。整句重写
//!   （前后缀之外差异过大）不会变成一条「把整句 A 换成整句 B」的规则——那种规则
//!   既不可能复用，还会在别的句子里误伤。
//! - **学错要改得掉**：同一 `wrong` 学出不同 `right` 时，历史支持度（`hits`）高的
//!   留下；平手时新手改法替换旧映射（用户此刻的纠正才是他想要的）。
//! - **落盘失败不能弄坏已有文件**：写盘一律「先写 `.tmp` 再 rename」，见 [`TypoMemory::save`]。
//!
//! # 上限
//!
//! [`MAX_PAIRS`] 条：手改错字是长尾行为，无界增长只会让 [`TypoMemory::apply`] 的
//! 线性扫描越来越慢。超出时按 `hits` **升序**淘汰——只学过一次的偶发改动最可能是
//! 误学（一次措辞调整、一次临时改写），先丢它们。

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// 一条替换对：`wrong`（识别错的模样）→ `right`（用户改成的样子）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypoPair {
    /// 错的那一段（空串对 `apply` 没有意义，读取时会被丢掉）。
    #[serde(default)]
    pub wrong: String,
    /// 对的那一段。
    #[serde(default)]
    pub right: String,
    /// 这条映射被用户「确认」过多少次。同一条重复学习只累加它。
    #[serde(default)]
    pub hits: u32,
}

/// 记忆条数上限。为什么有界：`apply` 是逐句线性扫描，库越大每条字幕越慢；而错别字
/// 是长尾数据，两千条已经远超正常用户会手改的量。
pub const MAX_PAIRS: usize = 2000;

/// 单个替换片段的长度上限（按 `char` 计，不是字节）。
///
/// 为什么是 12：中文错别字绝大多数是 1~4 字，专有名词/英文术语也就十来个字符。放宽
/// 到整句长度会让「一次改写」被学成一条大规则，在别的句子里必然误伤。
const MAX_PAIR_CHARS: usize = 12;

/// 错别字记忆库。内部就是一个 `Vec`，按 `wrong` 线性查找（两千条以内足够快，
/// 换成 `HashMap` 反而要为「长串优先」再维护一份有序视图，两处持久化容易不一致）。
#[derive(Debug, Clone, Default)]
pub struct TypoMemory {
    pairs: Vec<TypoPair>,
}

impl TypoMemory {
    /// 空表。
    pub fn new() -> Self {
        Self::default()
    }

    /// 从磁盘读取。**任何异常都返回空表，绝不报错**：这是启动路径上的辅助状态，
    /// 一个坏文件没有资格让程序起不来（与 `utils::queue_store::load_queue` 同一口径）。
    ///
    /// 读取时会顺手清掉手改 JSON 留下的空片段与超限条目：空 `wrong` 会让 [`apply`]
    /// 的扫描永远前进不了（死循环），必须在这里挡住。
    ///
    /// [`apply`]: TypoMemory::apply
    pub fn load(path: &Path) -> Self {
        // 缺失、无权限、非 UTF-8……统一当空表。
        let Ok(raw) = fs::read_to_string(path) else {
            return Self::new();
        };
        // 用编辑器手改这个文件时几乎都会带 BOM，serde_json 见到它直接解析失败，
        // 表现为「记忆库莫名清空」且没有任何报错可查。只剥开头这一个字符。
        let body = raw.strip_prefix('\u{FEFF}').unwrap_or(&raw);
        let Ok(mut pairs) = serde_json::from_str::<Vec<TypoPair>>(body) else {
            return Self::new();
        };
        pairs.retain(|p| !p.wrong.is_empty() && !p.right.is_empty());
        let mut memory = Self { pairs };
        memory.enforce_cap();
        memory
    }

    /// 原子写：先写 `<path>.tmp` 再 rename 覆盖。
    ///
    /// 为什么不能直接 `fs::write`：它会先截断原文件，进程恰好在这一刻被强杀，用户攒
    /// 了几周的错字表就变成 0 字节。rename 在同一目录内是原子的，最坏结果是「还是
    /// 上一版」。
    pub fn save(&self, path: &Path) -> Result<()> {
        // 首次运行 / 目录被用户清过：不先建目录会以「找不到路径」失败。
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("创建错别字记忆目录失败: {:?}", parent))?;
            }
        }

        // 紧凑 JSON：这是机器读的，缩进只会撑大文件、拖慢启动读取。
        let content = serde_json::to_string(&self.pairs).with_context(|| "序列化错别字记忆失败")?;

        let tmp = tmp_path(path);
        fs::write(&tmp, content.as_bytes())
            .with_context(|| format!("写入错别字记忆临时文件失败: {:?}", tmp))?;
        if let Err(err) = fs::rename(&tmp, path) {
            // 失败时清掉半成品，免得用户目录里留下一个看着像「已落盘」的垃圾文件。
            let _ = fs::remove_file(&tmp);
            return Err(err).with_context(|| format!("覆盖错别字记忆文件失败: {:?}", path));
        }
        Ok(())
    }

    /// 从一次编辑（整句 `before` → 整句 `after`）里提取最多一个替换对，返回学到的
    /// 条数（0 或 1）。
    ///
    /// 规则见模块文档：公共前后缀求差异区间、长度 1..=12、忽略纯标点/空白/数字、
    /// 忽略只差大小写的改动。任何一条不满足就安静丢弃——误学的代价是**用户之后的
    /// 每一句字幕都被改坏**，比漏学一条严重得多。
    pub fn learn_from_edit(&mut self, before: &str, after: &str) -> usize {
        let Some((wrong, right)) = diff_fragment(before, after) else {
            return 0;
        };
        if !is_usable_pair(&wrong, &right) {
            return 0;
        }
        self.record(wrong, right);
        1
    }

    /// 把整句里的 `wrong` 全部替换成 `right`；没有任何命中时返回 `None`。
    ///
    /// 两条硬要求：
    ///
    /// - **长串优先**：`机器` 与 `机器学习` 同时命中时先换长的，否则短规则会把长规则
    ///   的上下文切碎，换成半截结果；
    /// - **不重扫替换结果**：一次性从左到右扫描，命中就跳过整个 `wrong`，新写进去的
    ///   `right` 不会再被别的规则处理（否则 `甲→乙`、`乙→丙` 会让 `甲` 变成 `丙`，
    ///   两条规则连锁跑飞）。
    pub fn apply(&self, text: &str) -> Option<String> {
        // 先筛掉不可能命中的条目，再按字符数降序排——同一位置上永远先试最长的规则。
        let mut candidates: Vec<&TypoPair> = self
            .pairs
            .iter()
            .filter(|p| !p.wrong.is_empty() && text.contains(&p.wrong))
            .collect();
        if candidates.is_empty() {
            return None;
        }
        candidates.sort_by(|a, b| {
            b.wrong
                .chars()
                .count()
                .cmp(&a.wrong.chars().count())
                // 等长时按字典序定序，保证同一份库在任何机器上产出相同文本（测试可复现）。
                .then_with(|| a.wrong.cmp(&b.wrong))
        });

        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        let mut changed = false;
        while !rest.is_empty() {
            if let Some(pair) = candidates
                .iter()
                .find(|p| rest.starts_with(p.wrong.as_str()))
            {
                out.push_str(&pair.right);
                // 整个 `wrong` 一起跳过：替换出来的内容绝不参与后续匹配。
                rest = &rest[pair.wrong.len()..];
                changed = true;
                continue;
            }
            let ch = rest.chars().next().expect("rest 非空由循环条件保证");
            out.push(ch);
            rest = &rest[ch.len_utf8()..];
        }
        changed.then_some(out)
    }

    /// 当前条数。
    pub fn len(&self) -> usize {
        self.pairs.len()
    }

    /// 是否还没有任何记忆。
    pub fn is_empty(&self) -> bool {
        self.pairs.is_empty()
    }

    /// 全部条目（顺序不保证按学习先后，只保证与实现内部一致）。
    pub fn pairs(&self) -> &[TypoPair] {
        &self.pairs
    }

    /// 记一条映射：`(wrong, right)` 相同则累加 `hits`；`wrong` 相同但 `right` 不同时
    /// 留 `hits` 高的，平手时用新 `right` 覆盖（用户此刻的纠正优先）。
    fn record(&mut self, wrong: String, right: String) {
        if let Some(existing) = self.pairs.iter_mut().find(|p| p.wrong == wrong) {
            if existing.right == right {
                existing.hits = existing.hits.saturating_add(1);
            } else if existing.hits <= 1 {
                // 平手（都是「只确认过一次」）：新手改法生效。`hits` 不累加——冲突的
                // 两次修改不能算作同一映射的两次支持，否则改错一次也会显得「有共识」。
                existing.right = right;
            }
            // `existing.hits > 1`：旧映射有更多历史支持，保留，且同样不累加。
            return;
        }
        self.pairs.push(TypoPair {
            wrong,
            right,
            hits: 1,
        });
        self.enforce_cap();
    }

    /// 超出 [`MAX_PAIRS`] 时按 `hits` 升序淘汰。同 `hits` 时**先出现的先丢**（稳定
    /// 排序），保证结果只取决于写入顺序，不随排序实现漂移。
    fn enforce_cap(&mut self) {
        if self.pairs.len() <= MAX_PAIRS {
            return;
        }
        let mut ranked: Vec<usize> = (0..self.pairs.len()).collect();
        ranked.sort_by_key(|&i| self.pairs[i].hits);
        let excess = self.pairs.len() - MAX_PAIRS;
        let mut drop = vec![false; self.pairs.len()];
        for &i in &ranked[..excess] {
            drop[i] = true;
        }
        let mut kept = Vec::with_capacity(MAX_PAIRS);
        for (i, pair) in self.pairs.drain(..).enumerate() {
            if !drop[i] {
                kept.push(pair);
            }
        }
        self.pairs = kept;
    }
}

/// 计算同目录临时文件名：在完整文件名后追加 `.tmp`。
///
/// 不用 `Path::with_extension`：对没有扩展名或文件名里带点的路径，它的行为会变，
/// 可能把临时文件写到别处（`utils::queue_store` 里有同样的取舍说明）。
fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".tmp");
    PathBuf::from(name)
}

/// 用公共前后缀求最长差异区间。
///
/// 例：`我们明天去天安门广场` → `我们明天去天安门厂场`，去掉公共前缀
/// `我们明天去天安门` 与公共后缀 `场`，得到 `(广, 厂)`。
///
/// 前后缀刻意**不重叠**：一句 `我` → `我啊` 这种纯追加，差异区间是空串，按规则丢弃
/// （它说明不了「哪个字写错了」，只会学出一条「把 `我` 换成 `我啊`」的灾难规则）。
fn diff_fragment(before: &str, after: &str) -> Option<(String, String)> {
    if before == after {
        return None;
    }
    let b: Vec<char> = before.chars().collect();
    let a: Vec<char> = after.chars().collect();

    let shorter = b.len().min(a.len());
    let mut prefix = 0;
    while prefix < shorter && b[prefix] == a[prefix] {
        prefix += 1;
    }
    // 后缀最多只能吃到「前缀之外」的剩余长度，否则前后缀会在中间重叠，把差异区间
    // 算成空串（短句上尤其明显）。
    let remaining = shorter - prefix;
    let mut suffix = 0;
    while suffix < remaining && b[b.len() - 1 - suffix] == a[a.len() - 1 - suffix] {
        suffix += 1;
    }

    let wrong: String = b[prefix..b.len() - suffix].iter().collect();
    let right: String = a[prefix..a.len() - suffix].iter().collect();
    if wrong.is_empty() || right.is_empty() || wrong == right {
        return None;
    }
    Some((wrong, right))
}

/// 差异片段是否值得学成一条规则。
fn is_usable_pair(wrong: &str, right: &str) -> bool {
    let chars = wrong.chars().count();
    if chars == 0 || chars > MAX_PAIR_CHARS {
        return false;
    }
    let chars = right.chars().count();
    if chars == 0 || chars > MAX_PAIR_CHARS {
        return false;
    }
    // 句子级编辑不该跨行：换行进了规则，替换会在字幕里制造出凭空多出来的一行。
    if wrong.contains(['\n', '\r']) || right.contains(['\r', '\n']) {
        return false;
    }
    // 纯标点 / 纯空白 / 纯数字不是错别字：用户只是换了标点、删了空格、改了年份。
    // 学下来会把之后所有正常句子里的同类字符一起换掉。
    if is_noise_fragment(wrong) || is_noise_fragment(right) {
        return false;
    }
    // 只差大小写（`openai` → `OpenAI`）不是错别字，是排版偏好；中英混排的句子里
    // 这种差异极多，学下来会互相打架。
    wrong.to_lowercase() != right.to_lowercase()
}

/// 片段是否「只有标点 / 空白 / 数字」。
fn is_noise_fragment(fragment: &str) -> bool {
    fragment.chars().all(char::is_whitespace)
        || fragment.chars().all(is_punctuation)
        || fragment.chars().all(|c| c.is_numeric())
}

/// 标点判定（ASCII 标点 + 中文全角标点）。
///
/// 没有引入 Unicode 分类表：这里只需要覆盖字幕里实际会出现的那些符号，宁可少判几个
/// 生僻符号，也不为它多背一个依赖。
fn is_punctuation(c: char) -> bool {
    c.is_ascii_punctuation() || "，。、；：？！“”‘’（）【】《》〈〉「」『』…—～·•￥".contains(c)
}

#[cfg(test)]
mod tests {
    // 与 `utils::glossary` 的测试同一口径：只依赖公开接口，实现改名不会连累测试。
    use super::{TypoMemory, MAX_PAIRS};
    use std::path::PathBuf;

    /// 每条测试各自一个临时文件：并行跑测试时共用文件名会互相清掉对方的产物。
    fn temp_file(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "voice2word_typo_memory_{}_{}.json",
            tag,
            std::process::id()
        ))
    }

    /// 中文：公共前后缀之间的那个字就是 wrong。
    #[test]
    fn learns_chinese_typo_between_common_affixes() {
        let mut memory = TypoMemory::new();
        let learned = memory.learn_from_edit("我们明天去天安门广场", "我们明天去天安门厂场");
        assert_eq!(learned, 1);
        assert_eq!(memory.len(), 1);
        assert_eq!(memory.pairs()[0].wrong, "广");
        assert_eq!(memory.pairs()[0].right, "厂");
        assert_eq!(memory.pairs()[0].hits, 1);
    }

    /// 英文：整词替换同样靠前后缀定位。
    #[test]
    fn learns_english_typo_between_common_affixes() {
        let mut memory = TypoMemory::new();
        assert_eq!(memory.learn_from_edit("the cat sat", "the dog sat"), 1);
        assert_eq!(memory.pairs()[0].wrong, "cat");
        assert_eq!(memory.pairs()[0].right, "dog");
    }

    /// 没有差异 / 纯追加 / 纯删除都不学。
    ///
    /// Why：纯追加的差异区间是空串，把它当规则会学出「`我` → `我啊`」这种在别处
    /// 必然误伤的映射；纯删除的 `right` 为空，`apply` 会把所有出现处直接抹掉。
    #[test]
    fn ignores_empty_diff_and_pure_insert_delete() {
        let mut memory = TypoMemory::new();
        assert_eq!(memory.learn_from_edit("同一句话", "同一句话"), 0);
        assert_eq!(memory.learn_from_edit("我", "我啊"), 0);
        assert_eq!(memory.learn_from_edit("我啊", "我"), 0);
        assert_eq!(memory.learn_from_edit("", ""), 0);
        assert_eq!(memory.len(), 0, "一条都不该学进来");
    }

    /// 同一个 (wrong, right) 重复学习只累加 hits，不新增条目。
    #[test]
    fn repeated_learning_accumulates_hits() {
        let mut memory = TypoMemory::new();
        assert_eq!(memory.learn_from_edit("机器学习", "机哭学习"), 1);
        assert_eq!(memory.learn_from_edit("机器学习", "机哭学习"), 1);
        assert_eq!(memory.learn_from_edit("机器学习", "机哭学习"), 1);
        assert_eq!(memory.len(), 1, "同一条映射只能占一行");
        assert_eq!(memory.pairs()[0].hits, 3);
    }

    /// 同一个 wrong 学出不同 right：hits 高的留下，平手时新手改法替换。
    ///
    /// 三次编辑的公共前后缀都落在 `机` 与 `学习` 上，差异区间固定是 `器`，于是三次
    /// 学到的都是「`器` → 某个字」，正好构成冲突场景。
    #[test]
    fn conflicting_right_keeps_higher_hits_and_replaces_on_tie() {
        // hits=2 的旧映射不会被只学一次的新写法挤掉（旧的是用户反复确认过的）。
        let mut memory = TypoMemory::new();
        memory.learn_from_edit("他说机器学习", "他说机哭学习");
        memory.learn_from_edit("他说机器学习", "他说机哭学习");
        memory.learn_from_edit("他说机器学习", "他说机气学习");
        assert_eq!(memory.len(), 1);
        assert_eq!(memory.pairs()[0].wrong, "器");
        assert_eq!(memory.pairs()[0].right, "哭", "高 hits 的映射必须保留");
        assert_eq!(memory.pairs()[0].hits, 2, "冲突的修改不算支持，不累加");

        // 平手（都只学过一次）：新的纠正生效。
        let mut fresh = TypoMemory::new();
        fresh.learn_from_edit("他说机器学习", "他说机哭学习");
        fresh.learn_from_edit("他说机器学习", "他说机气学习");
        assert_eq!(fresh.len(), 1);
        assert_eq!(fresh.pairs()[0].right, "气", "平手时用新手改法");
    }

    /// `apply` 长串优先：短规则不能把长规则的上下文切碎。
    #[test]
    fn apply_prefers_longer_wrong_first() {
        let mut memory = TypoMemory::new();
        memory.learn_from_edit("我在深度学习", "我在DL");
        memory.learn_from_edit("我在学习", "我在study");
        // 两条都命中了 `深度学习`，必须先换长的，否则会得到「DL」被再处理或半截结果。
        assert_eq!(
            memory.apply("我在深度学习"),
            Some("我在DL".to_string()),
            "长串必须优先"
        );
        assert_eq!(memory.apply("我在学习"), Some("我在study".to_string()));
    }

    /// 替换结果不会被再扫一遍（`甲→乙`、`乙→丙` 不该连锁成 `甲→丙`）。
    #[test]
    fn apply_does_not_rescan_replaced_fragment() {
        let mut memory = TypoMemory::new();
        memory.learn_from_edit("甲", "乙");
        memory.learn_from_edit("乙", "丙");
        assert_eq!(memory.apply("甲"), Some("乙".to_string()));
        assert_eq!(memory.apply("甲乙"), Some("乙丙".to_string()));
    }

    /// 无关句子原样返回 None；有命中则替换。
    #[test]
    fn apply_leaves_unrelated_text_unchanged() {
        let mut memory = TypoMemory::new();
        memory.learn_from_edit("机器学习", "机哭学习");
        assert_eq!(memory.apply("今天天气不错"), None, "无关句子必须返回 None");
        assert_eq!(memory.apply(""), None);
        assert_eq!(
            memory.apply("我在学机器学习"),
            Some("我在学机哭学习".to_string())
        );
        // 命中就要如实返回替换后的文本（不是原句）。
        assert_eq!(
            memory.apply("机器学习"),
            Some("机哭学习".to_string()),
            "命中时必须返回改好的文本"
        );
    }

    /// 持久化往返：save 写出 JSON 数组、load 读回同样的映射与 hits。
    #[test]
    fn save_and_load_round_trip() {
        let path = temp_file("round_trip");
        let _ = std::fs::remove_file(&path);

        let mut memory = TypoMemory::new();
        memory.learn_from_edit("机器学习", "机哭学习");
        memory.learn_from_edit("机器学习", "机哭学习");
        memory.learn_from_edit("深度学习", "深度学系");
        memory.save(&path).expect("保存应当成功");

        let raw = std::fs::read_to_string(&path).expect("文件必须写出来");
        assert!(raw.starts_with('['), "持久化格式必须是 JSON 数组：{raw}");
        assert!(raw.contains("\"wrong\":\"器\""), "字段名固定：{raw}");
        assert!(raw.contains("\"hits\":2"), "hits 必须落盘：{raw}");
        let tmp = path.with_file_name(format!(
            "{}.tmp",
            path.file_name().unwrap().to_string_lossy()
        ));
        assert!(!tmp.exists(), "成功保存后不许残留 .tmp");

        let loaded = TypoMemory::load(&path);
        assert_eq!(loaded.len(), memory.len());
        assert_eq!(loaded.pairs(), memory.pairs(), "往返必须逐条相等");
        assert_eq!(
            loaded.apply("深度学习"),
            Some("深度学系".to_string()),
            "读回来的映射必须能直接应用"
        );

        let _ = std::fs::remove_file(&path);
    }

    /// 文件缺失 / JSON 损坏 / 形状不对 → 空表，不 panic、不报错。
    #[test]
    fn load_of_missing_or_corrupt_file_is_empty() {
        let missing = temp_file("missing");
        let _ = std::fs::remove_file(&missing);
        assert_eq!(TypoMemory::load(&missing).len(), 0);

        let broken = temp_file("broken");
        std::fs::write(&broken, b"not json at all").unwrap();
        assert_eq!(TypoMemory::load(&broken).len(), 0, "坏文件必须当空表");

        let wrong_shape = temp_file("shape");
        std::fs::write(&wrong_shape, br#"{"wrong":"a"}"#).unwrap();
        assert_eq!(TypoMemory::load(&wrong_shape).len(), 0, "不是数组就当空表");

        let _ = std::fs::remove_file(&broken);
        let _ = std::fs::remove_file(&wrong_shape);
    }

    /// 纯标点 / 纯空白 / 纯数字 / 只差大小写都不学。
    ///
    /// Why：这些改动每天都在发生（补个句号、删个空格、改个年份、英文大小写），学进
    /// 替换表会让之后每一句正常字幕都被改坏。
    #[test]
    fn ignores_punctuation_whitespace_digits_and_case_only_changes() {
        let mut memory = TypoMemory::new();
        assert_eq!(memory.learn_from_edit("你好，世界", "你好。世界"), 0, "标点");
        assert_eq!(memory.learn_from_edit("2024年", "2025年"), 0, "数字");
        assert_eq!(memory.learn_from_edit("说 话", "说话"), 0, "空白");
        assert_eq!(memory.learn_from_edit("openai", "OpenAI"), 0, "仅大小写");
        assert_eq!(memory.len(), 0, "噪声一条都不该学进来");
    }

    /// 超过上限按 hits 升序淘汰；反复确认过的映射必须活下来。
    #[test]
    fn cap_evicts_lowest_hits_first() {
        // 先用字母编码下标，保证每次学到的 (wrong, right) 都不同（用数字编码会被
        // 前后缀当成公共部分切掉）。
        fn tag(mut i: usize) -> String {
            let mut out = String::new();
            loop {
                out.push((b'a' + (i % 26) as u8) as char);
                i /= 26;
                if i == 0 {
                    return out;
                }
            }
        }

        let mut memory = TypoMemory::new();
        // 一条反复确认的映射：hits 最高，任何淘汰策略下都不该被丢。
        memory.learn_from_edit("固定开头甲固定结尾", "固定开头乙固定结尾");
        memory.learn_from_edit("固定开头甲固定结尾", "固定开头乙固定结尾");

        let total = MAX_PAIRS + 7;
        for i in 0..total {
            // 两侧各带一个不同的尾字符（`q` / `z`），保证差异区间一定包含 tag、且
            // 两侧都非空——若前后缀把 tag 吃干净，学到的就会是同一条映射。
            let (wrong, right) = (format!("{}q", tag(i)), format!("{}z", tag(i + 100_000)));
            memory.learn_from_edit(
                &format!("固定开头{wrong}固定结尾"),
                &format!("固定开头{right}固定结尾"),
            );
        }

        assert_eq!(memory.len(), MAX_PAIRS, "长度必须钳在上限");
        assert!(
            memory
                .pairs()
                .iter()
                .any(|p| p.wrong == "甲" && p.hits == 2),
            "hits 高的映射不许被淘汰"
        );
        assert!(
            !memory
                .pairs()
                .iter()
                .any(|p| p.wrong == format!("{}q", tag(0))),
            "hits 最低且最早出现的映射应先被丢"
        );
        assert!(
            memory
                .pairs()
                .iter()
                .any(|p| p.wrong == format!("{}q", tag(total - 1))),
            "最后学的一条必须还在"
        );
    }
}
