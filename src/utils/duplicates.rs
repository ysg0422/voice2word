//! 库内重复媒体分组：找出「同一份内容、被反复转写」的记录，交给界面提示。
//!
//! # 要防的失败模式：把同一场讲座又转写一遍
//!
//! 重新转写一份已经处理过的视频，是用户在本程序里能犯的**最贵**的错误：一场
//! 40 分钟的讲座要烧掉数分钟的 CPU/GPU，而这件事**天天发生**——文件被改了名、
//! 被复制到另一个目录、重新下载了一遍，或者用户单纯忘了自己转过。旧的库界面
//! 只按文件名罗列，上面这些情况看上去就是几条互不相干的记录，用户点「开始」时
//! 没有任何东西拦他一下。
//!
//! 数据库其实早就存了内容指纹（`content_hash`，见 [`crate::utils::fingerprint`]），
//! 它天然能穿透改名与复制——但在此之前没有任何地方**用过**它来在开转之前提醒。
//! 本模块补上的正是这一层。
//!
//! # 本模块只分组、不删除
//!
//! 这里不碰数据库、不删文件：它把「哪些库记录其实是同一份媒体」算出来交给界面，
//! 由界面决定是提示、高亮，还是让用户自己点「清理」。这样分组逻辑可以脱离 DB
//! 单测（见 [`TaskRecordLike`]），也不会因为界面的调用时机而在后台产生副作用。
//!
//! # 为什么「指纹未知」不能凑成一组
//!
//! 老记录、读取失败的文件都没有指纹。若把一堆「未知」塞进同一组，界面会显示
//! 「这些是同一份文件」，而它们其实毫无关系——**误导比不提示更糟**。因此
//! `None` 与空串一律排除在分组之外，见 [`group_by_fingerprint`]。

use crate::storage::db::TaskRecord;
use std::collections::hash_map::Entry;
use std::collections::HashMap;

/// 一组「同一份媒体」的库记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateGroup {
    /// 内容指纹（同组共用）
    pub fingerprint: String,
    /// 组内记录 id（按输入顺序，稳定）
    pub ids: Vec<i64>,
    /// 组内记录的文件名（与 `ids` 一一对应，便于界面直接显示）
    pub names: Vec<String>,
}

/// 供本模块使用的**最小字段视图**。
///
/// 为什么不直接吃 [`TaskRecord`]：那个结构还带 `segments: Vec<Segment>`，而分组
/// 只需要 id / 文件名 / 指纹。收窄成 trait 让本模块不依赖 storage 层，单测可以
/// 凭空造数据（不建库、不碰磁盘），也让「分组逻辑」与「数据库」解耦——以后换
/// 存储实现（或从快照反序列化出的记录）都能直接复用同一套分组规则。
pub trait TaskRecordLike {
    /// 库内主键：界面用它定位记录、执行清理。
    fn id(&self) -> i64;
    /// 展示给用户看的名字（库记录的 `file_name`，不是完整路径）。
    fn display_name(&self) -> String;
    /// 内容指纹；`None` 表示「指纹未知」（老记录或指纹计算失败）。
    fn content_hash(&self) -> Option<&str>;
}

impl TaskRecordLike for TaskRecord {
    fn id(&self) -> i64 {
        self.id
    }

    fn display_name(&self) -> String {
        self.file_name.clone()
    }

    fn content_hash(&self) -> Option<&str> {
        self.content_hash.as_deref()
    }
}

/// 按内容指纹把库记录分组，只返回**多于一条**的组。
///
/// - 没有指纹的记录（`content_hash` 为空 / `None`，即老记录或指纹计算失败的）**不参与
///   分组**：把一堆「指纹未知」的记录塞进同一组会让用户以为它们是同一份文件，
///   那比不提示更糟。
/// - 组内顺序按输入顺序（调用方通常已按时间倒序给出，界面直接沿用）。
/// - 组之间的顺序：按组内第一条记录的输入顺序，保证界面稳定（不做哈希序，否则
///   每次打开顺序都变）。
///
/// 泛型 `T: TaskRecordLike` 是文档里 `&[TaskRecordLike]` 的可编译写法：调用方
/// 直接传 `&Vec<TaskRecord>` 或 `&[FakeRecord]` 都行，无需装箱成 trait 对象。
pub fn group_by_fingerprint<T: TaskRecordLike>(records: &[T]) -> Vec<DuplicateGroup> {
    // `index` 只负责「指纹 → 组号」的 O(1) 查找；输出的组间顺序完全由 `groups`
    // 的插入顺序决定，绝不依赖 HashMap 的迭代顺序，因此对同一输入结果稳定。
    let mut index: HashMap<&str, usize> = HashMap::new();
    let mut groups: Vec<DuplicateGroup> = Vec::new();

    for record in records {
        // `None` 与空串都表示「指纹未知」，一律跳过：它们既不进组，也不会因为
        // 「都是未知」而被并到一起（见模块文档里的失败模式）。
        let Some(fingerprint) = record.content_hash() else {
            continue;
        };
        if fingerprint.is_empty() {
            continue;
        }

        // 用 `entry` 一次哈希完成「查 + 插」，避免 `get` 之后再 `insert` 的重复
        // 查找（clippy 也会提示 `map_entry`）。
        match index.entry(fingerprint) {
            Entry::Occupied(entry) => {
                let slot = *entry.get();
                let group = &mut groups[slot];
                group.ids.push(record.id());
                group.names.push(record.display_name());
            }
            Entry::Vacant(entry) => {
                entry.insert(groups.len());
                groups.push(DuplicateGroup {
                    fingerprint: fingerprint.to_string(),
                    ids: vec![record.id()],
                    names: vec![record.display_name()],
                });
            }
        }
    }

    // 单条记录算不上「重复」。`retain` 保持插入顺序，所以组间顺序仍是各组首条的
    // 输入顺序——若改用哈希序，用户每次打开库看到的组顺序都会变。
    groups.retain(|group| group.ids.len() > 1);
    groups
}

/// 给界面用的一句话摘要，例如
/// "发现 3 组重复（共 7 条记录，可清理 4 条）" / "没有发现重复记录"。
///
/// 三个数都现场从 `groups` 算出，调用方不必自己维护计数：
/// - 组数 = 有多少份媒体被重复入库；
/// - 总记录数 = 这些组一共占了多少条库记录；
/// - 可清理数 = 每组留一条之后剩余的总数（= 总记录数 − 组数）。
///
/// 空输入返回固定文案「没有发现重复记录」，界面可直接用它判断要不要显示提示条。
/// 这批记录里有几条带指纹——也就是**真正参与比对**的条数。
///
/// 为什么要单独暴露它：`describe` 只会说「有没有重复」，而「一条都没比过」与
/// 「比过了、没重复」在用户看来必须区分开。库里的老记录（或文件已不可读的记录）
/// 指纹为 `None`，会被 [`group_by_fingerprint`] 跳过；此时若照直说「没有发现重复
/// 记录」，就是在把**没测**说成**通过**——用户会以为自己的重复项目已经被确认过。
pub fn fingerprinted_count<T: TaskRecordLike>(records: &[T]) -> usize {
    records
        .iter()
        .filter(|record| {
            record
                .content_hash()
                .is_some_and(|fingerprint| !fingerprint.is_empty())
        })
        .count()
}

/// 查重的用户可见结论（纯函数，便于单测）。
///
/// 与 [`describe`] 的区别：先回答「这次到底比没比过」。`fingerprinted` 为 0 时
/// 绝不能给出「没有重复」这种像结论的话。
pub fn describe_scan(groups: &[DuplicateGroup], fingerprinted: usize) -> String {
    if fingerprinted == 0 {
        return "库里还没有可供比对的指纹（老记录没有指纹，或文件已不可读），暂时无法查重"
            .to_string();
    }
    describe(groups)
}
pub fn describe(groups: &[DuplicateGroup]) -> String {
    if groups.is_empty() {
        return "没有发现重复记录".to_string();
    }

    let total: usize = groups.iter().map(|group| group.ids.len()).sum();
    // 用「建议清理数」累加，而不是 `total - groups.len()`：万一有调用方手搓出
    // 空组，减法会下溢 panic。逐组相加天然安全，本函数永不 panic。
    let removable: usize = groups
        .iter()
        .map(|group| suggested_removals(group).len())
        .sum();

    format!(
        "发现 {} 组重复（共 {} 条记录，可清理 {} 条）",
        groups.len(),
        total,
        removable
    )
}

/// 某一组里「建议保留」的记录 id：**保留最早入库的那条**（即 id 最小的那条）。
///
/// 为什么不整组清掉：先入库的那条往往已经落过库、甚至人工校对过，后来的重转更
/// 可能是无意的；把整组删了等于连用户的校对成果一起抹掉。
///
/// 这里**不能**取 `ids[0]`：本模块的输入约定是「按时间倒序」（真实的调用方
/// `Database::list_recent_tasks` 就是 `ORDER BY id DESC`），组内第一条恰恰是
/// **最新**的那条——按它保留会把最早、最可能已人工校对过的那条建议删掉。因此
/// 显式取 id 最小者，与输入顺序无关。
/// 组内只有一个成员时返回它自己；空组返回 `None`（不 panic）。
pub fn suggested_keeper(group: &DuplicateGroup) -> Option<i64> {
    group.ids.iter().copied().min()
}

/// 某一组里建议**清理**的 id（即除 keeper 之外的全部），保持组内输入顺序。
///
/// keeper 是「id 最小」的那条，可能并不在组内首位（输入按时间倒序），所以不能
/// 简单地 `skip(1)`：必须显式跳过 keeper 本身，同时保持其余记录的输入顺序。
pub fn suggested_removals(group: &DuplicateGroup) -> Vec<i64> {
    let keeper_index = group
        .ids
        .iter()
        .enumerate()
        .min_by_key(|(_, id)| **id)
        .map(|(index, _)| index);

    group
        .ids
        .iter()
        .enumerate()
        .filter(|(index, _)| Some(*index) != keeper_index)
        .map(|(_, id)| *id)
        .collect()
}

/// 查重结论的「没测 ≠ 通过」判据。
#[cfg(test)]
mod describe_scan_tests {
    use super::{describe_scan, fingerprinted_count};
    use crate::storage::TaskRecord;

    fn task(id: i64, fingerprint: Option<&str>) -> TaskRecord {
        TaskRecord {
            id,
            file_path: format!("D:/media/{id}.mp4"),
            file_name: format!("{id}.mp4"),
            duration: 1.0,
            status: "completed".to_string(),
            created_at: "2026-01-01 00:00:00".to_string(),
            segments: Vec::new(),
            segment_count: 0,
            sample_text: String::new(),
            segments_loaded: false,
            has_translation: false,
            content_hash: fingerprint.map(str::to_string),
            metrics: None,
        }
    }

    /// 一条指纹都没有：结论必须说「无法查重」，绝不能说「没有重复」。
    #[test]
    fn no_fingerprints_is_not_a_clean_bill() {
        let rows = vec![task(1, None), task(2, Some("")), task(3, None)];
        assert_eq!(fingerprinted_count(&rows), 0);
        let msg = describe_scan(&[], fingerprinted_count(&rows));
        assert!(msg.contains("无法查重"), "{msg}");
        assert!(!msg.contains("没有发现重复"), "不能说成结论：{msg}");
    }

    /// 比过了、没重复：这时「没有发现重复记录」才是真话。
    #[test]
    fn scanned_and_clean_says_no_duplicates() {
        let rows = vec![task(1, Some("aaa")), task(2, Some("bbb"))];
        assert_eq!(fingerprinted_count(&rows), 2);
        assert_eq!(
            describe_scan(&[], fingerprinted_count(&rows)),
            "没有发现重复记录"
        );
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    /// 单测用的最小记录：只带分组需要的三个字段，不建库、不碰磁盘。
    ///
    /// 这正是 [`TaskRecordLike`] 存在的理由——分组逻辑不必拖着
    /// `Vec<Segment>` 和 SQLite 一起跑。
    struct FakeRecord {
        id: i64,
        name: String,
        hash: Option<String>,
    }

    impl FakeRecord {
        /// 造一条带有效指纹的记录。
        fn with_hash(id: i64, name: &str, hash: &str) -> Self {
            Self {
                id,
                name: name.to_string(),
                hash: Some(hash.to_string()),
            }
        }

        /// 造一条指纹为 `None` 的记录（老记录 / 读取失败）。
        fn unknown(id: i64, name: &str) -> Self {
            Self {
                id,
                name: name.to_string(),
                hash: None,
            }
        }

        /// 造一条指纹为空串的记录（另一种「未知」）。
        fn empty_hash(id: i64, name: &str) -> Self {
            Self {
                id,
                name: name.to_string(),
                hash: Some(String::new()),
            }
        }
    }

    impl TaskRecordLike for FakeRecord {
        fn id(&self) -> i64 {
            self.id
        }

        fn display_name(&self) -> String {
            self.name.clone()
        }

        fn content_hash(&self) -> Option<&str> {
            self.hash.as_deref()
        }
    }

    /// 两条记录指纹相同 → 归为一组，指纹、id、文件名都要一一对上。
    #[test]
    fn same_fingerprint_forms_one_group() {
        let records = vec![
            FakeRecord::with_hash(1, "lecture.mp4", "aa"),
            FakeRecord::with_hash(2, "lecture - 副本.mp4", "aa"),
        ];

        let groups = group_by_fingerprint(&records);

        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].fingerprint, "aa");
        assert_eq!(groups[0].ids, vec![1, 2]);
        assert_eq!(groups[0].names, vec!["lecture.mp4", "lecture - 副本.mp4"]);
    }

    /// 三个互不相同的指纹 → 没有任何重复，返回空列表。
    #[test]
    fn distinct_fingerprints_yield_no_groups() {
        let records = vec![
            FakeRecord::with_hash(1, "a.mp4", "aa"),
            FakeRecord::with_hash(2, "b.mp4", "bb"),
            FakeRecord::with_hash(3, "c.mp4", "cc"),
        ];

        assert!(group_by_fingerprint(&records).is_empty());
    }

    /// 重复与单条混在一起：只返回重复组，且组间顺序 = 各组首条的输入顺序。
    #[test]
    fn only_duplicate_groups_are_returned_in_input_order() {
        let records = vec![
            FakeRecord::with_hash(1, "single.mp4", "solo"),
            FakeRecord::with_hash(2, "dup_a.mp4", "dup1"),
            FakeRecord::with_hash(3, "dup_b.mp4", "dup2"),
            FakeRecord::with_hash(4, "dup_a2.mp4", "dup1"),
            FakeRecord::with_hash(5, "dup_b2.mp4", "dup2"),
            FakeRecord::with_hash(6, "dup_a3.mp4", "dup1"),
        ];

        let groups = group_by_fingerprint(&records);

        assert_eq!(groups.len(), 2);
        // dup1 的首条（id=2）先于 dup2 的首条（id=3）出现，因此 dup1 组在前。
        assert_eq!(groups[0].fingerprint, "dup1");
        assert_eq!(groups[0].ids, vec![2, 4, 6]);
        assert_eq!(groups[1].fingerprint, "dup2");
        assert_eq!(groups[1].ids, vec![3, 5]);
    }

    /// `None` 与 `""` 都表示「指纹未知」：既不参与分组，也不会被并到一起。
    #[test]
    fn unknown_fingerprints_are_excluded_and_never_grouped() {
        let records = vec![
            FakeRecord::unknown(1, "old_none_a.mp4"),
            FakeRecord::unknown(2, "old_none_b.mp4"),
            FakeRecord::empty_hash(3, "old_empty_a.mp4"),
            FakeRecord::empty_hash(4, "old_empty_b.mp4"),
            FakeRecord::with_hash(5, "real.mp4", "aa"),
            FakeRecord::with_hash(6, "real_copy.mp4", "aa"),
        ];

        let groups = group_by_fingerprint(&records);

        // 只有真正有指纹的 "aa" 成组；四条未知记录一条都不该冒出来。
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].fingerprint, "aa");
        assert_eq!(groups[0].ids, vec![5, 6]);
    }

    /// 组内顺序严格跟随输入顺序（界面按这个顺序显示，不能被打乱）。
    #[test]
    fn group_internal_order_follows_input() {
        let records = vec![
            FakeRecord::with_hash(30, "c.mp4", "aa"),
            FakeRecord::with_hash(10, "a.mp4", "aa"),
            FakeRecord::with_hash(20, "b.mp4", "aa"),
        ];

        let groups = group_by_fingerprint(&records);

        assert_eq!(groups[0].ids, vec![30, 10, 20]);
        assert_eq!(groups[0].names, vec!["c.mp4", "a.mp4", "b.mp4"]);
    }

    /// 指纹比较是精确字符串相等：大小写不同就是不同指纹。本程序生成的一律小写，
    /// 这里不做、也不能做大小写折叠，否则可能把两份不同的媒体并成一组。
    #[test]
    fn fingerprint_comparison_is_case_sensitive() {
        let records = vec![
            FakeRecord::with_hash(1, "upper.mp4", "AABB"),
            FakeRecord::with_hash(2, "lower.mp4", "aabb"),
        ];

        assert!(group_by_fingerprint(&records).is_empty());
    }

    /// 空输入不 panic，返回空组列表。
    #[test]
    fn empty_input_yields_no_groups() {
        let records: Vec<FakeRecord> = Vec::new();

        assert!(group_by_fingerprint(&records).is_empty());
    }

    /// `describe` 的空态文案固定，界面可直接拿它判断要不要显示提示条。
    #[test]
    fn describe_empty_input_has_fixed_text() {
        assert_eq!(describe(&[]), "没有发现重复记录");
    }

    /// `describe` 的三个计数（组数 / 总记录数 / 可清理数）都要对得上。
    #[test]
    fn describe_counts_groups_records_and_removals() {
        let records = vec![
            FakeRecord::with_hash(1, "a.mp4", "dup1"),
            FakeRecord::with_hash(2, "a2.mp4", "dup1"),
            FakeRecord::with_hash(3, "a3.mp4", "dup1"),
            FakeRecord::with_hash(4, "b.mp4", "dup2"),
            FakeRecord::with_hash(5, "b2.mp4", "dup2"),
            FakeRecord::with_hash(6, "c.mp4", "dup3"),
            FakeRecord::with_hash(7, "c2.mp4", "dup3"),
        ];

        let groups = group_by_fingerprint(&records);

        // 3 组、共 7 条、每组留 1 条 → 可清理 4 条。
        assert_eq!(
            describe(&groups),
            "发现 3 组重复（共 7 条记录，可清理 4 条）"
        );
    }

    /// keeper 取 id 最小（最早入库）的那条；单成员组返回它自己；空组返回 `None`。
    #[test]
    fn suggested_keeper_handles_all_group_shapes() {
        let records = vec![
            FakeRecord::with_hash(11, "first.mp4", "aa"),
            FakeRecord::with_hash(22, "second.mp4", "aa"),
        ];
        let groups = group_by_fingerprint(&records);
        assert_eq!(suggested_keeper(&groups[0]), Some(11));

        let single = DuplicateGroup {
            fingerprint: "aa".to_string(),
            ids: vec![7],
            names: vec!["only.mp4".to_string()],
        };
        assert_eq!(suggested_keeper(&single), Some(7));

        let empty = DuplicateGroup {
            fingerprint: "aa".to_string(),
            ids: Vec::new(),
            names: Vec::new(),
        };
        assert_eq!(suggested_keeper(&empty), None);
    }

    /// removals = 除 keeper 之外的全部 id，保持组内输入顺序。
    #[test]
    fn suggested_removals_returns_the_rest_in_order() {
        let group = DuplicateGroup {
            fingerprint: "aa".to_string(),
            ids: vec![5, 9, 13],
            names: vec![
                "a.mp4".to_string(),
                "b.mp4".to_string(),
                "c.mp4".to_string(),
            ],
        };

        assert_eq!(suggested_removals(&group), vec![9, 13]);
    }

    /// 真实调用方（`Database::list_recent_tasks` 是 `ORDER BY id DESC`）按时间**倒序**
    /// 给出记录，组内第一条是**最新**的那条。keeper 必须是**最早**入库（id 最小）的那条，
    /// 否则「查重」会建议删掉用户可能已人工校对过的老记录——这正是一条真实路径。
    #[test]
    fn suggested_keeper_prefers_the_earliest_record_not_the_newest() {
        let records = vec![
            FakeRecord::with_hash(30, "newest.mp4", "aa"),
            FakeRecord::with_hash(10, "oldest.mp4", "aa"),
            FakeRecord::with_hash(20, "middle.mp4", "aa"),
        ];
        let groups = group_by_fingerprint(&records);

        // 分组本身不重排：组内顺序仍严格按输入（时间倒序）。
        assert_eq!(groups[0].ids, vec![30, 10, 20]);
        assert_eq!(
            suggested_keeper(&groups[0]),
            Some(10),
            "保留最早入库的那条，而不是组内第一条"
        );
        assert_eq!(
            suggested_removals(&groups[0]),
            vec![30, 20],
            "其余按输入顺序列出，且不含 keeper"
        );
        // 计数仍与「每组留一条」一致。
        assert_eq!(
            describe(&groups),
            "发现 1 组重复（共 3 条记录，可清理 2 条）"
        );
    }

    /// 同一份输入连算两次必须完全一致：输出里不能混进 HashMap 的迭代顺序。
    #[test]
    fn grouping_is_deterministic() {
        let records: Vec<FakeRecord> = (0..40)
            .map(|i| FakeRecord::with_hash(i, &format!("f{i}.mp4"), &format!("fp{}", i % 7)))
            .collect();

        let first = group_by_fingerprint(&records);
        let second = group_by_fingerprint(&records);

        assert_eq!(first, second);
    }

    /// 200 条记录分布在 50 个指纹上：分组必须正确，且不能退化成 O(n²)。
    ///
    /// 指纹按 `i % 50` 轮转，让同一指纹的记录在输入里互相交错——组内顺序必须仍是
    /// 输入顺序，组间顺序仍是各组首条的顺序。
    #[test]
    fn large_input_groups_correctly() {
        let records: Vec<FakeRecord> = (0..200)
            .map(|i| FakeRecord::with_hash(i, &format!("clip{i}.mp4"), &format!("fp{}", i % 50)))
            .collect();

        let groups = group_by_fingerprint(&records);

        assert_eq!(groups.len(), 50);
        for (slot, group) in groups.iter().enumerate() {
            assert_eq!(group.fingerprint, format!("fp{slot}"));
            let expected: Vec<i64> = (0..4).map(|k| slot as i64 + 50 * k).collect();
            assert_eq!(group.ids, expected);
        }
    }
}
