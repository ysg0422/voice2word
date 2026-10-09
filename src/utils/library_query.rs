//! 视频库查询层：对「已经加载好的一整页记录」做关键字过滤、翻译状态过滤与排序。
//!
//! # 要防的失败模式：40 条记录，只能靠滚动条一条条找
//!
//! 视频库把每一条工程渲染成一张卡片，平铺在一个**没有虚拟化**的滚动列表里。记录
//! 一多（40+ 条很常见），唯一的检索手段就是滚动：想找「上个月做的那场讲座」或者
//! 「还没翻译的那几条」，只能逐张卡片读文件名、看有没有译文。这不是「不够方便」，
//! 而是**功能缺失**——库里明明存着这些信息，界面却没有入口去问它。
//!
//! # 为什么单独做一层纯查询，而不是直接写进视图
//!
//! 过滤与排序是**纯函数**：输入一批记录 + 一组条件，输出一个顺序。把它从 GPUI 视图
//! 里拆出来（[`query`] 不碰 DB、不认识 `gpui`），换来两件确定的好处：
//!
//! 1. 逻辑可以脱离窗口单测——造几条假记录就能把「大小写不敏感」「稳定排序」这些
//!    边界钉死，而不是靠肉眼看界面；
//! 2. 界面每帧重绘时只拿缓存的 id 顺序去原表取记录，不会因为过滤逻辑混进渲染代码
//!    而出现「同一份数据两处各算一遍、结果不一致」。
//!
//! # 返回 id 而不是记录本身
//!
//! [`query`] 只返回 `Vec<i64>`（记录 id 的顺序），不克隆记录。原因很实在：
//! [`crate::storage::db::TaskRecord`] 里挂着 `segments: Vec<Segment>`，40 条 ×
//! 上千句的克隆是肉眼可见的浪费，而界面**只需要顺序**——按 id 去原表取那一条即可。
//!
//! # 与 storage 层解耦
//!
//! 与 [`crate::utils::duplicates`] 同样的理由：本模块只通过 [`LibraryRecord`] 这个
//! **最小字段视图**看记录，不依赖存储层的具体类型。这样单测能凭空造数据（不建库、
//! 不碰磁盘），以后换存储实现（或从快照反序列化出的记录）也能复用同一套查询规则。
//!
//! # 为什么是「朴素子串」而不是模糊匹配
//!
//! 命中判定只有一条：关键字（忽略首尾空白、忽略大小写）是文件名或完整路径的**连续
//! 子串**。不做模糊/分词/拼音匹配——在一个 40 行的列表里，模糊匹配带来的**意外命中
//! 远多于帮助**：用户输入 `test` 却把 `latest_notes.mp4` 也留下，他会开始怀疑筛选
//! 是不是坏了。宁可漏、不要错，用户多打几个字符的成本远低于「筛出来的东西不对」。
//!
//! # 大小写与 CJK 的排序口径
//!
//! [`LibrarySort::NameAsc`] 是「先转小写再按字节序比较」。对拉丁字母这就是 A→Z、
//! 且不区分大小写；对 CJK 没有大小写可言，UTF-8 的字节序恰好等于码点序，因此
//! 它就是**码点顺序**（不是拼音、不是笔画、也不是 locale 感知的 collation）。这里
//! 刻意不假装是「智能排序」：locale 相关的比较需要引入 ICU 级别的依赖，而本程序的
//! 文件名绝大多数是英文/数字，收益远小于复杂度。

use crate::storage::db::TaskRecord;

/// 库记录的排序方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LibrarySort {
    /// 最近处理在前（默认，与 `list_recent_tasks` 的顺序一致）
    #[default]
    NewestFirst,
    OldestFirst,
    /// 名称 A→Z（不区分大小写）
    NameAsc,
    /// 句数从多到少（长片在前）
    SegmentsDesc,
}

impl LibrarySort {
    /// 全部可选项（界面遍历用；顺序即下拉里的顺序）。
    ///
    /// 顺序固定为「默认项在前，其余按用户最可能用到的先后」，界面直接遍历本数组
    /// 生成下拉，不要在视图里另写一份列表——两份清单一旦分叉，新增排序方式时
    /// 总有一处会漏。
    pub const ALL: [LibrarySort; 4] = [
        LibrarySort::NewestFirst,
        LibrarySort::OldestFirst,
        LibrarySort::NameAsc,
        LibrarySort::SegmentsDesc,
    ];

    /// 界面用中文名。
    ///
    /// 每个标签都必须是**非空且互不相同**的字符串：下拉里出现两个一样的选项，
    /// 用户就无法判断当前选中的是哪一个（单测钉住了这条）。
    pub fn label(self) -> &'static str {
        match self {
            LibrarySort::NewestFirst => "最新优先",
            LibrarySort::OldestFirst => "最早优先",
            LibrarySort::NameAsc => "名称 A→Z",
            LibrarySort::SegmentsDesc => "句数从多到少",
        }
    }
}

/// 过滤条件。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LibraryFilter {
    /// 关键字：匹配**文件名或完整路径**，不区分大小写，首尾空白忽略。
    /// 空串 = 不过滤。
    pub query: String,
    /// 只显示「还没翻译」的记录（`has_translation` 为 false 的）
    pub untranslated_only: bool,
    /// 只显示「已翻译」的记录
    pub translated_only: bool,
}

impl LibraryFilter {
    /// 是否处于「什么都没过滤」的状态（界面据此决定要不要显示「清除筛选」）。
    ///
    /// 只把**纯空白**的关键字算作「没填」：用户在搜索框里敲了几个空格又清掉，
    /// 不该被当成一个生效的筛选条件而让「清除筛选」按钮一直亮着。
    pub fn is_empty(&self) -> bool {
        self.query.trim().is_empty() && !self.untranslated_only && !self.translated_only
    }
}

/// 记录的最小字段视图（同 [`crate::utils::duplicates`] 的理由：不依赖 storage 层，
/// 单测可造数据）。
///
/// 各字段的语义与「为什么界面需要它」：
/// - [`LibraryRecord::id`]：主键，界面拿它回原表取记录；
/// - [`LibraryRecord::display_name`]：卡片上显示的名字，也是关键字匹配的第一目标；
/// - [`LibraryRecord::path`]：完整路径，关键字匹配的第二目标（见下方说明）；
/// - [`LibraryRecord::has_translation`]：翻译状态过滤的依据；
/// - [`LibraryRecord::segment_count`]：句数排序的依据。
pub trait LibraryRecord {
    /// 库内主键。
    fn id(&self) -> i64;

    /// 展示给用户看的名字（库记录的 `file_name`，不是完整路径）。
    fn display_name(&self) -> String;

    /// 完整路径（搜索也匹配它——用户常记得目录名而不是文件名）。
    fn path(&self) -> String;

    /// 该记录是否已有译文。
    fn has_translation(&self) -> bool;

    /// 该记录的字幕句数。
    fn segment_count(&self) -> usize;
}

impl LibraryRecord for TaskRecord {
    fn id(&self) -> i64 {
        self.id
    }

    fn display_name(&self) -> String {
        self.file_name.clone()
    }

    fn path(&self) -> String {
        self.file_path.clone()
    }

    /// 是否已有译文。
    ///
    /// `TaskRecord` 本身**没有**翻译状态字段，唯一的真实信号在 `segments` 里
    /// （逐句 [`crate::subtitle::Segment::has_translation`]）。这里用「任一句有译文」
    /// 判定，也就是与 `AppState::translated_count > 0` 同一口径。
    ///
    /// # 已知边界：列表查询的记录没有正文
    ///
    /// `list_recent_tasks` 刻意**不反序列化** `segments`（见 `storage::db` 的说明），
    /// 所以列表记录的空壳永远返回 `false`。这带来两个后果，都在这里明说、不假装没有：
    /// - 「只看未翻译」在列表记录上恒等于「全部」；
    /// - 「只看已翻译」在列表记录上会得到空列表。
    ///
    /// 想要精确结果，调用方要么用带正文的记录（`segments_loaded == true`），要么给
    /// 自己的包装类型实现 [`LibraryRecord`] 并从别处（例如另查一次译文计数）取状态。
    /// 之所以不在这里偷偷「猜」（比如看 `sample_text`），是因为**猜错的状态会让筛选
    /// 结果静默丢记录**，比明确地按「未知即未翻译」处理更难排查。
    fn has_translation(&self) -> bool {
        self.segments
            .iter()
            .any(|segment| segment.has_translation())
    }

    fn segment_count(&self) -> usize {
        self.segment_count
    }
}

/// 按条件过滤并排序，返回**记录 id 的顺序**（不克隆记录本身）。
///
/// 为什么返回 id 而不是记录切片：`TaskRecord` 含 `segments: Vec<Segment>`，克隆整表
/// 在 40 条 × 上千句时是明显的浪费；界面只需要顺序，按 id 去原表取即可。
///
/// # 过滤规则
///
/// - 关键字：`filter.query` 去掉首尾空白、转小写后，作为**连续子串**去匹配文件名
///   **或**完整路径（两者都忽略大小写）。空关键字不过滤。
/// - 翻译状态：`untranslated_only` / `translated_only` 各自生效；两者**同时为真**
///   时视为「不过滤」（见下）。
///
/// # 两个翻译开关同时为真 = 不过滤（而不是空列表）
///
/// 界面上两个开关是互斥的，理论上不会同时为真；但本函数是纯逻辑，不能指望调用方
/// 永远守规矩。此时若返回空列表，用户看到的是「筛选后一条都没有」，会以为**库空了
/// 或者筛选坏了**——那比不做筛选糟得多。因此这里选择「都设了就都不生效」，与
/// [`LibraryFilter::is_empty`] 的判定保持一致（`is_empty` 也把这种组合算作「有筛选」，
/// 但实际效果等于全量，界面会显示「共 N 项 · 显示 N 项」，一眼能看出没起作用）。
///
/// # 排序稳定性
///
/// 用 [`slice::sort_by`]：它在 Rust 标准库里是**稳定**排序，键相等时保持输入顺序。
/// 这一点是刻意的——DB 的 `list_recent_tasks` 已按 `id DESC` 返回（最近在前），
/// 稳定性让「同名」「同句数」这些并列项自动沿用「最近在前」作为次序，用户不会
/// 看到两次打开顺序不一样。
///
/// # 永不 panic
///
/// `records` 为空切片时返回空 `Vec`；所有比较与字符串操作都不会 panic。
pub fn query<T: LibraryRecord>(
    records: &[T],
    filter: &LibraryFilter,
    sort: LibrarySort,
) -> Vec<i64> {
    // 关键字归一：去首尾空白 + 转小写，只做一次，避免每条记录重复处理。
    let needle = filter.query.trim().to_lowercase();

    // 两个开关都开（或都不开）→ 不做翻译状态过滤。
    let translation_wanted: Option<bool> = match (filter.untranslated_only, filter.translated_only)
    {
        (true, false) => Some(false),
        (false, true) => Some(true),
        _ => None,
    };

    let mut matched: Vec<&T> = records
        .iter()
        .filter(|record| {
            if let Some(want_translated) = translation_wanted {
                if record.has_translation() != want_translated {
                    return false;
                }
            }
            if needle.is_empty() {
                return true;
            }
            record
                .display_name()
                .to_lowercase()
                .contains(needle.as_str())
                || record.path().to_lowercase().contains(needle.as_str())
        })
        .collect();

    // `sort_by` 是稳定排序（Rust 标准库保证）：键相等的记录保持输入顺序。
    // 不要改成 `sort_unstable_by`——那会打乱并列项，用户每次打开看到的顺序可能不同。
    matched.sort_by(|a, b| match sort {
        // 主键是 AUTOINCREMENT，`list_recent_tasks` 就是 `ORDER BY id DESC`，
        // 因此 id 倒序 == 时间倒序。显式按 id 排而不是「信任输入顺序」：即使调用方
        // 传进来的是过滤/合并后的临时向量，结果也仍然正确且可复现。
        LibrarySort::NewestFirst => b.id().cmp(&a.id()),
        LibrarySort::OldestFirst => a.id().cmp(&b.id()),
        // 先转小写再按字节序比较：拉丁字母 A→Z 且不分大小写；CJK 无大小写，
        // UTF-8 字节序即码点序（见模块文档）。
        LibrarySort::NameAsc => a
            .display_name()
            .to_lowercase()
            .cmp(&b.display_name().to_lowercase()),
        LibrarySort::SegmentsDesc => b.segment_count().cmp(&a.segment_count()),
    });

    matched.iter().map(|record| record.id()).collect()
}

/// 界面用的一句话摘要，例如
/// "共 42 项 · 显示 7 项" / "共 42 项"（未过滤时不啰嗦）。
///
/// `total` 是过滤前的总数、`shown` 是过滤后的条数，都由调用方传入：本函数只负责
/// 措辞，不碰记录，因此可以脱离任何数据源单测。
///
/// 「有没有过滤」一律以 [`LibraryFilter::is_empty`] 为准，而不是 `total != shown`：
/// 用户输入了关键字但恰好全部命中时，仍应显示「共 N 项 · 显示 N 项」——他才知道
/// 筛选是生效的、只是没有排除任何一条。
pub fn describe(total: usize, shown: usize, filter: &LibraryFilter) -> String {
    if filter.is_empty() {
        format!("共 {total} 项")
    } else {
        format!("共 {total} 项 · 显示 {shown} 项")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// 单测用的最小记录：只带查询需要的五个字段，不建库、不碰磁盘。
    ///
    /// 这正是 [`LibraryRecord`] 存在的理由——过滤/排序逻辑不必拖着
    /// `Vec<Segment>` 和 SQLite 一起跑。
    struct FakeRecord {
        id: i64,
        name: String,
        path: String,
        translated: bool,
        segments: usize,
    }

    impl FakeRecord {
        /// 造一条记录：`translated` 决定「有没有译文」，`segments` 是句数。
        fn new(id: i64, name: &str, path: &str, translated: bool, segments: usize) -> Self {
            Self {
                id,
                name: name.to_string(),
                path: path.to_string(),
                translated,
                segments,
            }
        }
    }

    impl LibraryRecord for FakeRecord {
        fn id(&self) -> i64 {
            self.id
        }

        fn display_name(&self) -> String {
            self.name.clone()
        }

        fn path(&self) -> String {
            self.path.clone()
        }

        fn has_translation(&self) -> bool {
            self.translated
        }

        fn segment_count(&self) -> usize {
            self.segments
        }
    }

    /// 固定样本：输入顺序就是「最近在前」（id 5→1 递减），与 `list_recent_tasks` 同序。
    ///
    /// - id 5 / id 3：名字里都有 "lecture"，句数同为 10（并列，用于稳定排序）；
    /// - id 3 的目录名是 `Backup`，文件名里没有（用于「只命中路径」用例）；
    /// - id 4 / id 1：句数同为 30（并列）；
    /// - id 4 / id 2：已有译文。
    fn sample() -> Vec<FakeRecord> {
        vec![
            FakeRecord::new(5, "Lecture.mp4", "D:/Courses/2026/lecture.mp4", false, 10),
            FakeRecord::new(4, "Notes.mp4", "D:/Misc/notes.mp4", true, 30),
            FakeRecord::new(3, "lecture - 副本.mp4", "E:/Backup/lecture.mp4", false, 10),
            FakeRecord::new(2, "apple.mp4", "D:/Fruit/apple.mp4", true, 7),
            FakeRecord::new(1, "Banana.mp4", "D:/Fruit/banana.mp4", false, 30),
        ]
    }

    /// 空筛选 + 默认排序：所有 id 原样按输入顺序返回（默认排序与 `list_recent_tasks` 同序）。
    #[test]
    fn empty_filter_returns_all_ids_in_input_order() {
        let records = sample();

        let ids = query(
            &records,
            &LibraryFilter::default(),
            LibrarySort::NewestFirst,
        );

        assert_eq!(ids, vec![5, 4, 3, 2, 1]);
    }

    /// 关键字命中文件名：不区分大小写，且过滤条件里的首尾空白要被忽略。
    #[test]
    fn query_matches_name_case_insensitively_and_ignores_surrounding_whitespace() {
        let records = sample();
        let filter = LibraryFilter {
            query: "  LECTURE  ".to_string(),
            ..LibraryFilter::default()
        };

        let ids = query(&records, &filter, LibrarySort::NewestFirst);

        // 5 与 3 的文件名都含 "lecture"（大小写不同），空白被 trim 掉后仍然命中。
        assert_eq!(ids, vec![5, 3]);
    }

    /// 关键字只出现在**目录名**里（文件名不含）：必须命中路径，否则用户记得目录却搜不到。
    #[test]
    fn query_matches_directory_name_in_path_only() {
        let records = sample();
        let filter = LibraryFilter {
            query: "backup".to_string(),
            ..LibraryFilter::default()
        };

        let ids = query(&records, &filter, LibrarySort::NewestFirst);

        // 只有 id 3 的路径含 "Backup"，它的文件名 "lecture - 副本.mp4" 并不含该词。
        assert_eq!(ids, vec![3]);
    }

    /// 关键字谁都不命中 → 空列表（不是报错、也不是全量）。
    #[test]
    fn query_without_match_returns_empty() {
        let records = sample();
        let filter = LibraryFilter {
            query: "zzz-not-here".to_string(),
            ..LibraryFilter::default()
        };

        assert!(query(&records, &filter, LibrarySort::NewestFirst).is_empty());
    }

    /// `untranslated_only` 只留没有译文的记录。
    #[test]
    fn untranslated_only_keeps_records_without_translation() {
        let records = sample();
        let filter = LibraryFilter {
            untranslated_only: true,
            ..LibraryFilter::default()
        };

        let ids = query(&records, &filter, LibrarySort::NewestFirst);

        assert_eq!(ids, vec![5, 3, 1]);
    }

    /// `translated_only` 只留有译文的记录。
    #[test]
    fn translated_only_keeps_records_with_translation() {
        let records = sample();
        let filter = LibraryFilter {
            translated_only: true,
            ..LibraryFilter::default()
        };

        let ids = query(&records, &filter, LibrarySort::NewestFirst);

        assert_eq!(ids, vec![4, 2]);
    }

    /// 两个翻译开关**同时**为真（界面本该避免，纯函数必须安全）→ 视为「不过滤」。
    ///
    /// 这是文档里钉住的选择：返回空列表会看起来像「筛选坏了 / 库空了」。
    #[test]
    fn both_translation_flags_set_disable_translation_filter() {
        let records = sample();
        let filter = LibraryFilter {
            untranslated_only: true,
            translated_only: true,
            ..LibraryFilter::default()
        };

        let ids = query(&records, &filter, LibrarySort::NewestFirst);

        assert_eq!(ids, vec![5, 4, 3, 2, 1], "两个开关同开应等于不做翻译过滤");
    }

    /// `NewestFirst` 对「已经是最近在前」的输入是恒等；`OldestFirst` 是它的逆序。
    #[test]
    fn newest_first_is_identity_and_oldest_first_is_its_reverse() {
        let records = sample();

        let newest = query(
            &records,
            &LibraryFilter::default(),
            LibrarySort::NewestFirst,
        );
        let oldest = query(
            &records,
            &LibraryFilter::default(),
            LibrarySort::OldestFirst,
        );

        assert_eq!(newest, vec![5, 4, 3, 2, 1]);
        assert_eq!(oldest, vec![1, 2, 3, 4, 5]);
    }

    /// `NameAsc` 不区分大小写：`Banana` / `apple` 按小写后比较，而不是大写字母在前。
    #[test]
    fn name_asc_is_case_insensitive() {
        let records = sample();

        let ids = query(&records, &LibraryFilter::default(), LibrarySort::NameAsc);

        // 小写名：apple(2) < banana(1) < "lecture - 副本.mp4"(3) < lecture.mp4(5) < notes(4)。
        // 其中 3 与 5 在 "lecture" 后分叉：空格(0x20) < 点(0x2E)。
        assert_eq!(ids, vec![2, 1, 3, 5, 4]);
    }

    /// 稳定性：`NameAsc` 下名字相等（忽略大小写）的记录保持输入顺序。
    #[test]
    fn name_asc_keeps_input_order_for_equal_names() {
        let records = vec![
            FakeRecord::new(9, "Same.mp4", "D:/a/same.mp4", false, 1),
            FakeRecord::new(8, "same.mp4", "D:/b/same.mp4", false, 1),
            FakeRecord::new(7, "SAME.mp4", "D:/c/same.mp4", false, 1),
        ];

        let ids = query(&records, &LibraryFilter::default(), LibrarySort::NameAsc);

        assert_eq!(ids, vec![9, 8, 7], "键相等时 `sort_by` 必须保持输入顺序");
    }

    /// `SegmentsDesc` 句数多的在前，且并列时保持输入顺序（稳定排序）。
    #[test]
    fn segments_desc_sorts_by_count_and_keeps_input_order_on_ties() {
        let records = sample();

        let ids = query(
            &records,
            &LibraryFilter::default(),
            LibrarySort::SegmentsDesc,
        );

        // 句数：4=30, 1=30, 5=10, 3=10, 2=7。并列的 30 保持输入顺序 4→1，
        // 并列的 10 保持输入顺序 5→3。
        assert_eq!(ids, vec![4, 1, 5, 3, 2]);
    }

    /// 空记录切片：任何筛选与排序都返回空，且不 panic。
    #[test]
    fn query_handles_empty_records() {
        let records: Vec<FakeRecord> = Vec::new();
        let filter = LibraryFilter {
            query: "x".to_string(),
            untranslated_only: true,
            ..LibraryFilter::default()
        };

        for sort in LibrarySort::ALL {
            assert!(query(&records, &filter, sort).is_empty());
            assert!(query(&records, &LibraryFilter::default(), sort).is_empty());
        }
    }

    /// `describe` 的两种形态：过滤时给出「共 N 项 · 显示 M 项」，未过滤时只给总数。
    #[test]
    fn describe_exact_strings_filtered_and_unfiltered() {
        let by_query = LibraryFilter {
            query: "lecture".to_string(),
            ..LibraryFilter::default()
        };
        let by_state = LibraryFilter {
            translated_only: true,
            ..LibraryFilter::default()
        };

        assert_eq!(describe(42, 7, &by_query), "共 42 项 · 显示 7 项");
        assert_eq!(describe(42, 7, &by_state), "共 42 项 · 显示 7 项");
        // 未过滤：不啰嗦地重复「显示 N 项」
        assert_eq!(describe(42, 42, &LibraryFilter::default()), "共 42 项");
        // 有筛选但全部命中：仍然显示两段，让用户知道筛选生效了
        assert_eq!(describe(42, 42, &by_query), "共 42 项 · 显示 42 项");
        // 空库
        assert_eq!(describe(0, 0, &LibraryFilter::default()), "共 0 项");
    }

    /// `ALL` 的顺序（即下拉里的顺序）固定，且默认项就是第一个。
    #[test]
    fn all_order_is_fixed_and_default_is_first() {
        assert_eq!(
            LibrarySort::ALL,
            [
                LibrarySort::NewestFirst,
                LibrarySort::OldestFirst,
                LibrarySort::NameAsc,
                LibrarySort::SegmentsDesc,
            ]
        );
        assert_eq!(LibrarySort::default(), LibrarySort::ALL[0]);
    }

    /// 每个 `label()` 非空且互不相同：下拉里出现重复标签，用户分不清选中了哪一个。
    #[test]
    fn every_label_is_non_empty_and_unique() {
        let mut seen: HashSet<&str> = HashSet::new();
        for sort in LibrarySort::ALL {
            let label = sort.label();
            assert!(!label.is_empty(), "{sort:?} 的中文名不能为空");
            assert!(seen.insert(label), "{label} 被两个排序方式共用了");
        }
        assert_eq!(seen.len(), LibrarySort::ALL.len());
    }

    /// `is_empty` 真值表：只有「关键字是纯空白」且两个开关都关时才算「没筛选」。
    #[test]
    fn is_empty_truth_table() {
        assert!(LibraryFilter::default().is_empty(), "全默认应算未过滤");
        assert!(
            LibraryFilter {
                query: "   ".to_string(),
                ..LibraryFilter::default()
            }
            .is_empty(),
            "纯空白关键字等于没填"
        );
        assert!(!LibraryFilter {
            query: "a".to_string(),
            ..LibraryFilter::default()
        }
        .is_empty());
        assert!(!LibraryFilter {
            untranslated_only: true,
            ..LibraryFilter::default()
        }
        .is_empty());
        assert!(!LibraryFilter {
            translated_only: true,
            ..LibraryFilter::default()
        }
        .is_empty());
        assert!(
            !LibraryFilter {
                untranslated_only: true,
                translated_only: true,
                ..LibraryFilter::default()
            }
            .is_empty(),
            "两个开关同开仍算「有筛选」，只是效果等于全量"
        );
    }
}
