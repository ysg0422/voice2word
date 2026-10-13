//! SQLite 数据库存储模块

use anyhow::{Context, Result};
use rusqlite::{params, Connection, ErrorCode, OptionalExtension, TransactionBehavior};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use tracing::{info, warn};

use crate::subtitle::Segment;

/// `SQLITE_BUSY` / `SQLITE_LOCKED` 的重试窗口与间隔。
///
/// 连接上已设了 `busy_timeout=5s`，但那只覆盖「等待加锁」；WAL 的
/// `checkpoint` 与「另一实例正在写」仍可能在等待耗尽后把错误直接抛上来。
/// 这类错误是**瞬时**的（对方的事务很快就结束），重试几次即可自愈；
/// 而任何其它错误（约束冲突、语法错误、库损坏）绝不能重试——那只会把
/// 「立刻失败」变成「转 5 秒再失败」，还多掩盖一层根因。
const BUSY_RETRY_ATTEMPTS: usize = 5;
const BUSY_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(120);

/// 是否是「库被占用」这类可重试的瞬时错误。
fn is_busy(err: &rusqlite::Error) -> bool {
    matches!(
        err,
        rusqlite::Error::SqliteFailure(e, _)
            if e.code == ErrorCode::DatabaseBusy || e.code == ErrorCode::DatabaseLocked
    )
}

/// 对可重试的「库被占用」错误做有限次重试，其余错误原样返回。
fn with_busy_retry<T, F>(mut op: F) -> rusqlite::Result<T>
where
    F: FnMut() -> rusqlite::Result<T>,
{
    let mut attempt = 0usize;
    loop {
        match op() {
            Ok(v) => return Ok(v),
            Err(err) if is_busy(&err) && attempt + 1 < BUSY_RETRY_ATTEMPTS => {
                attempt += 1;
                std::thread::sleep(BUSY_RETRY_INTERVAL);
            }
            Err(err) => return Err(err),
        }
    }
}

/// SQLite 的 `CURRENT_TIMESTAMP` 产出的 UTC 时间戳，格式 `YYYY-MM-DD HH:MM:SS`。
///
/// 迁移回填与显式插入都需要一个「和建表默认值同形状」的时间戳：
/// `library.rs` 按 `created_at[..10]` 切出 `YYYY-MM-DD` 做日期展示，格式必须与
/// SQLite 自己写的完全一致，否则同一条列表里会出现两种长相的日期。
/// （`ALTER TABLE ADD COLUMN` 只接受**常量**默认值，见 `apply_migrations`。）
fn utc_now() -> String {
    chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

/// 反序列化后归一「全空白」的分段字段。
///
/// `Segment` 的字符串字段在 JSON 里可能以**空白串**而不是 `null` / 空串落库
/// （旧版本、外部工具或界面误存）。这类值必须在装载时消掉：
/// - `translation` / `translation_lang` / `language`：空白串会让
///   `translation_matches()` 误判「已有译文且就是目标语言」，界面上出现
///   「已翻译」的空行，还会跳过真正需要的重译；
/// - `polished`：`display_text()` 只把「trim 后为空」的润色稿视为未润色，
///   归一成空串后与「从未润色」的语义完全一致，消费点也无需各自判空。
fn normalize_segment_blanks(segments: &mut [Segment]) {
    for seg in segments.iter_mut() {
        if seg
            .translation
            .as_deref()
            .is_some_and(|text| text.trim().is_empty())
        {
            seg.translation = None;
        }
        if seg
            .translation_lang
            .as_deref()
            .is_some_and(|lang| lang.trim().is_empty())
        {
            seg.translation_lang = None;
        }
        if seg
            .language
            .as_deref()
            .is_some_and(|lang| lang.trim().is_empty())
        {
            seg.language = None;
        }
        if seg.polished.trim().is_empty() {
            seg.polished = String::new();
        }
    }
}

/// 从内存字幕算出历史列表的三个派生摘要（随 `segments_json` 一起物化进列）。
///
/// 为什么在这里算、而不是继续让 `list_recent_tasks` 的 SQL 现算：SQL 侧的
/// `json_each` / `json_extract` 每次列表查询都要把每条工程的 `segments_json`
/// 完整展开解析一遍——一个几百句的工程就是数百 KB，50 条就是几 MB，且这段
/// 开销全在 UI 线程上。写入侧本来就拿着 `&[Segment]`，顺手算出来是零成本。
///
/// 口径必须与旧的 SQL 实现一致（那段 SQL 现在只保留在 v3→v4 的回填里）：
/// - `sample_text`：首句 `polished` 去掉 **ASCII 空白**后非空则取 trim 结果，
///   否则取首句 `text`；没有句子则为空串。不能图省事用 `display_text()`：
///   它返回未 trim 的 `polished`，而旧 SQL 返回 trim 后的值，两端会漂移。
/// - `has_translation`：复用 [`Segment::has_translation`]（全项目唯一的判定
///   口径，与 `hydrate_task` 装载后的重算完全一致；纯空白译文不算）。
/// - `segment_count`：句数。
fn summarize_segments(segments: &[Segment]) -> (usize, String, bool) {
    let sample_text = segments
        .first()
        .map(|seg| {
            let polished = seg.polished.trim_matches(|c: char| c.is_ascii_whitespace());
            if polished.is_empty() {
                seg.text.clone()
            } else {
                polished.to_string()
            }
        })
        .unwrap_or_default();
    let has_translation = segments.iter().any(Segment::has_translation);
    (segments.len(), sample_text, has_translation)
}

/// 迁移回填 `sample_text` 时 `TRIM(X, …)` 用的字符集：**ASCII 空白**。
///
/// 与写入侧同口径：`summarize_segments` 判 `polished` 是否为空用的是
/// `char::is_ascii_whitespace`，所以这里只能给 ASCII 空白集（`TRIM(X)` 默认只去半角空格，
/// 连 `\n` 都删不掉，必须显式列出）。
const SQL_ASCII_WHITESPACE_SET: &str =
    "' ' || char(9) || char(10) || char(11) || char(12) || char(13)";

/// 迁移回填 `has_translation` 时 `TRIM(X, …)` 用的字符集：**Unicode White_Space**。
///
/// # 为什么不能用 ASCII 集（本轮修的不一致）
///
/// `has_translation` 的口径是 [`Segment::has_translation`]（`str::trim`，按 Unicode
/// White_Space 属性判空）。若回填只去 ASCII 空白，那么「只由 NBSP(U+00A0) / 全角空格
/// (U+3000) / 行分隔符(U+2028) 这类**非 ASCII 空白**组成的译文」会被回填成 `1`（已翻译），
/// 而任何一次写入（`summarize_segments`）或装载（`normalize_segment_blanks` +
/// `hydrate_task` 重算）都会把它算成 `0`——同一份 `segments_json` 在两个口径下给出相反
/// 答案，列表页的「已翻译」徽标与打开工程后的实际状态对不上。
///
/// 集合逐字对应 Rust `char::is_whitespace` 覆盖的码点；SQLite 的 `char()` 接受码点、
/// `TRIM(X, Y)` 的 Y 按字符集处理，因此多字节字符不会被按字节误删。
const SQL_UNICODE_WHITESPACE_SET: &str = concat!(
    "' ' || char(9) || char(10) || char(11) || char(12) || char(13)",
    " || char(133) || char(160) || char(5760)",
    " || char(8192) || char(8193) || char(8194) || char(8195) || char(8196)",
    " || char(8197) || char(8198) || char(8199) || char(8200) || char(8201)",
    " || char(8202) || char(8232) || char(8233) || char(8239) || char(8287)",
    " || char(12288)",
);

#[derive(Debug, Clone)]
pub struct TaskRecord {
    pub id: i64,
    pub file_path: String,
    pub file_name: String,
    pub duration: f64,
    pub status: String,
    /// 字幕片段本体。
    ///
    /// 历史库列表查询（`list_recent_tasks`）**不**反序列化此字段：一个几百句的
    /// 工程动辄数百 KB JSON，启动时把 50 条全部解析会白白拖慢首屏。列表只用
    /// `segment_count` / `sample_text` / `has_translation` 三个由写入侧随
    /// `segments_json` 一起物化的摘要列；真正需要字幕内容时（载入工程、导出）
    /// 再按 `id` 调 `load_task_segments` 取。
    pub segments: Vec<Segment>,
    /// 该工程的字幕总句数（写入侧随 `segments_json` 物化，见 `summarize_segments`）
    pub segment_count: usize,
    /// 首句字幕摘录，供视频库卡片预览
    pub sample_text: String,
    /// `segments` 是否已装载。为 `false` 时表示来自列表查询，内容为空壳。
    pub segments_loaded: bool,
    /// 记录中是否**至少有一句带非空译文**（与 `Segment::has_translation` 同口径，
    /// 纯空白译文不算）。
    ///
    /// **列表查询的空壳记录也带对这项**：`insert_task` / `update_task_segments`
    /// 用 `Segment::has_translation` 的口径把它物化进列（`summarize_segments`），
    /// 列表记录直接读列；逐句装载路径（`row_to_task` / `hydrate_task`）则按
    /// 内存字幕重算。视频库的「仅已翻译 / 仅未翻译」筛选据此工作，
    /// 不会因正文未装载而退化成「恒空 / 恒全量」。
    pub has_translation: bool,
    pub created_at: String,
    /// 媒体内容的采样指纹（见 `utils::fingerprint`）。老记录或无法读取的文件为 `None`。
    /// 缓存的**主键**是它，而不是 `file_path`——同内容改名/复制能命中，同路径换内容
    /// 会正确地失效。
    pub content_hash: Option<String>,
    pub metrics: Option<crate::core::PipelinePerformanceMetrics>,
}

/// `segments_json` 解码失败的可读原因（带 task id 与原始长度）。
///
/// 库里的 `segments_json` 是**外部可改**的数据：历史版本写的旧结构、手动改库、
/// 外部工具、磁盘损坏都可能留下解不开的 JSON。旧实现把解码失败 `.ok()` 掉再
/// `unwrap_or_default()`，坏数据一律变成「这份工程没有字幕」——用户看到空白剪辑台，
/// 日志里也毫无线索。这个类型就是「把原因带回到调用方可读位置」的载体。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentDecodeError {
    /// 出问题的记录主键（日志与界面提示都靠它定位）
    pub task_id: i64,
    /// `segments_json` 的原始字节数（截断/损坏的直观证据）
    pub raw_len: usize,
    /// `serde_json` 的原始报错文本
    pub reason: String,
}

impl std::fmt::Display for SegmentDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "任务 {} 的字幕数据无法解析（{} 字节）: {}",
            self.task_id, self.raw_len, self.reason
        )
    }
}

impl std::error::Error for SegmentDecodeError {}

/// 字幕正文装载结果：`decode_error` 为 `Some` 时 `segments` 只是「尽力而为的空表」，
/// 调用方**必须**把原因呈现出来，不能当成「这份素材没有语音」。
#[derive(Debug, Clone, PartialEq)]
pub struct SegmentLoadReport {
    pub segments: Vec<Segment>,
    pub decode_error: Option<SegmentDecodeError>,
}

impl SegmentLoadReport {
    /// 是否装载失败（坏 `segments_json`）。
    pub fn failed(&self) -> bool {
        self.decode_error.is_some()
    }
}

/// 解码一行 `segments_json`，把「坏数据」与「本来就没有字幕」彻底分开。
///
/// - `None`（行不存在 / 列为 NULL）与空串：这份工程确实没有字幕 → 空表、无错误；
/// - 解码失败：空表 + `Some(err)`，并留一发带 task id 的 `warn!`。
///
/// 抽成纯函数是为了让单测能直接喂坏数据，不必真的去构造一个损坏的库文件。
fn decode_segments_json(
    task_id: i64,
    raw: Option<&str>,
) -> (Vec<Segment>, Option<SegmentDecodeError>) {
    let Some(raw) = raw else {
        return (Vec::new(), None);
    };
    if raw.trim().is_empty() {
        return (Vec::new(), None);
    }
    match serde_json::from_str::<Vec<Segment>>(raw) {
        Ok(segments) => (segments, None),
        Err(err) => {
            warn!(
                task_id,
                raw_len = raw.len(),
                error = %err,
                "历史库 segments_json 解析失败：这份工程的字幕无法装载（不再静默变成空表）"
            );
            (
                Vec::new(),
                Some(SegmentDecodeError {
                    task_id,
                    raw_len: raw.len(),
                    reason: err.to_string(),
                }),
            )
        }
    }
}

#[derive(Clone, Debug)]
pub struct Database {
    conn: Arc<Mutex<Connection>>,
}

impl Database {
    /// 取连接锁，**容忍 Mutex 中毒**。
    ///
    /// `Mutex::lock()` 在被持锁线程 panic 后会返回 `Err(Poisoned)`，而 `unwrap()`
    /// 会让此后**每一次** DB 访问都连环 panic——一个后台线程的小 bug 就能把整个
    /// 程序打死。中毒只说明「上次有人在持锁时 panic 了」，连接本身并没有损坏：
    /// SQLite 连接是独立的 C 结构，panic 发生在 Rust 侧、不会把它写成半截状态
    /// （进程内所有写入仍走 SQLite 自己的事务/回滚日志）。因此这里取回内部值继续
    /// 用，比连环 panic 安全得多。
    ///
    /// 全文件统一走这一个入口，`maintain_on_shutdown` 里原先那套
    /// `match lock() { Err(p) => p.into_inner() }` 也并进来，不留两套风格。
    fn lock_conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `tasks` 表是否已有名为 `column` 的列。
    ///
    /// `ALTER TABLE ADD COLUMN` **不幂等**：列已存在时它报 "duplicate column name"。
    /// 迁移必须能安全重跑（版本号错位、上次迁移失败后的重试等），所以补列前先探一次，
    /// 而不是「执行失败就当它已存在、把错误吞掉」。
    fn column_exists(conn: &Connection, column: &str) -> rusqlite::Result<bool> {
        let mut stmt = conn.prepare("PRAGMA table_info(tasks)")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let name: String = row.get(1)?;
            if name == column {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// 当前 schema 版本。每次「新增列 / 新增表 / 改索引」都必须 +1，并在
    /// [`Self::apply_migrations`] 里补一段从旧版本升到它的迁移。
    ///
    /// 为什么用 `PRAGMA user_version` 而不是继续 `ALTER TABLE ADD COLUMN` 硬试：
    /// 硬试的写法对「加列」能用，但一旦要做**结构性迁移**（回填、改索引、建新表，
    /// 或未来某次迁移必须严格只跑一次）就无从下手——它只会在列已存在时静默跳过、
    /// 在真出问题时也静默失败（返回值被 `let _ =` 吞掉）。版本号把「当前库处在哪个
    /// schema 阶段」变成可读、可断言的状态。
    pub const SCHEMA_VERSION: i32 = 4;

    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let conn = Connection::open(path)?;
        // ── 并发与耐久性设置 ──
        //
        // `busy_timeout`：本进程内所有访问都经同一个 `Mutex<Connection>`，看似不会
        // 冲突；但用户可能同时开着另一个实例、或外部工具（DB Browser 等）在读。
        // 没有这个设置时，SQLite 遇到锁会**立刻**返回 SQLITE_BUSY，表现为
        // 「写入随缘失败」。给 5 秒等待窗口，绝大多数瞬时锁都能自愈。
        //
        // `journal_mode=WAL`：默认的 rollback journal 在每次写入时要独占整个库，
        // 且崩溃恢复更慢。WAL 让读不阻塞写、写不阻塞读，崩溃后恢复也更快。
        // 返回值是实际生效的模式（内存库会回落成 memory），无需处理。
        let _ = conn.busy_timeout(std::time::Duration::from_secs(5));
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        // 平衡耐久性与速度：NORMAL 下只在 checkpoint 时 fsync，
        // 断电可能丢最后几个事务，但不会损坏数据库。字幕工程可接受
        // （丢的也只是最近一次编辑快照），换来的是写入明显更快。
        let _ = conn.pragma_update(None, "synchronous", "NORMAL");

        // ── 损坏自愈 ──
        // 库文件损坏（断电、磁盘坏道、外部工具写坏）时，`init_tables` 里的任何一条
        // 语句都会直接报错，`Database::open` 于是返回 Err，**整个程序启动失败**——
        // 用户连界面都进不去，历史库里的其它完好工程也一并不可达。
        //
        // 这里先用 `quick_check` 探一次：损坏则把原库改名留底（`.corrupt-<时间戳>`，
        // 绝不删除，用户可能还想用 sqlite3 .recover 抢救），再建一个空库继续启动。
        // 代价是「历史列表暂时空了」，收益是「程序能开、能继续干活、能导出」。
        // quick_check 在 512 KB 的库上实测约 7 ms，对启动无感。
        let db = match Self::heal_if_corrupt(conn) {
            Ok(conn) => Self {
                conn: Arc::new(Mutex::new(conn)),
            },
            Err(heal_err) => {
                // 自愈本身失败（例如目录只读、改名被拒）——退回原库错误，不掩盖根因。
                return Err(heal_err);
            }
        };
        db.init_tables()?;
        db.apply_migrations()?;
        Ok(db)
    }

    /// 检查连接并返回**可用**的连接：完好则原样返回；损坏则改名留底并新建空库。
    ///
    /// 传入的是已经设过 WAL/busy_timeout 的连接（这些 PRAGMA 对损坏库也能设置成功，
    /// 所以能走到这里）。
    fn heal_if_corrupt(conn: Connection) -> Result<Connection> {
        let healthy = conn
            .query_row("PRAGMA quick_check", [], |row| row.get::<_, String>(0))
            .map(|s| s == "ok")
            .unwrap_or(false);
        if healthy {
            return Ok(conn);
        }

        // 取原库路径用于改名与新建。`Connection::path()` 对内存库返回 None，
        // 内存库不可能「损坏到要备份」，直接放行。
        let path = match conn.path() {
            Some(p) if !p.is_empty() => std::path::PathBuf::from(p),
            _ => return Ok(conn),
        };
        warn!(path = %path.display(), "数据库 quick_check 未通过，尝试自愈：改名留底并新建空库");
        // 关掉 WAL，确保 -wal / -shm 在改名时一并处理干净（否则残留副产物会
        // 让新建的空库读到旧的未 checkpoint 事务）。
        let _ = conn.pragma_update(None, "journal_mode", "DELETE");
        drop(conn);

        let stamp = crate::utils::time::timestamp_for_filename();
        let backup = path.with_extension(format!("corrupt-{stamp}.db"));
        std::fs::rename(&path, &backup).with_context(|| {
            format!(
                "数据库损坏，但改名留底失败（{} → {}）",
                path.display(),
                backup.display()
            )
        })?;
        // 顺手把 WAL 副产物也改名留底，避免它们被新建的空库误当自己的日志。
        for suffix in ["-wal", "-shm"] {
            let side = std::path::PathBuf::from(format!("{}{}", path.display(), suffix));
            if side.exists() {
                let _ = std::fs::rename(
                    &side,
                    std::path::PathBuf::from(format!("{}{}", backup.display(), suffix)),
                );
            }
        }
        warn!(
            backup = %backup.display(),
            "已把损坏的数据库改名留底并新建空库；如仍想抢救旧数据，可用 sqlite3 对备份执行 .recover"
        );
        let fresh = Connection::open(&path)?;
        let _ = fresh.busy_timeout(std::time::Duration::from_secs(5));
        let _ = fresh.pragma_update(None, "journal_mode", "WAL");
        let _ = fresh.pragma_update(None, "synchronous", "NORMAL");
        Ok(fresh)
    }

    fn init_tables(&self) -> Result<()> {
        let conn = self.lock_conn();
        conn.execute(
            r#"
            CREATE TABLE IF NOT EXISTS tasks (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                file_path TEXT NOT NULL,
                file_name TEXT NOT NULL,
                duration REAL DEFAULT 0.0,
                status TEXT NOT NULL,
                segments_json TEXT NOT NULL,
                content_hash TEXT,
                metrics_json TEXT,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                segment_count INTEGER NOT NULL DEFAULT 0,
                sample_text TEXT NOT NULL DEFAULT '',
                has_translation INTEGER NOT NULL DEFAULT 0
            );
            "#,
            [],
        )?;
        // 历史补列（`metrics_json` 等）**不在这里做**：补列属于 schema 迁移，统一由
        // `apply_migrations` 在 `BEGIN IMMEDIATE` 事务里完成。
        //
        // 为什么不能顺手在这里 `column_exists` + `ALTER TABLE ADD COLUMN`：那是
        // 事务外的 check-then-act。两个实例同时打开同一个**老库**时，双方都可能探到
        // 「列不存在」，随后各发一条 ALTER，后到的那条以 `duplicate column name`
        // 失败——它是逻辑错误、不触发 busy 重试，`open` 于是返回 Err，程序连界面都
        // 进不去。放进迁移事务后，先拿锁的一方改完，另一方等锁、拿锁后重新探列，
        // 看到列已在即跳过，天然无竞态（单进程行为不变：老库仍由 v0→v1 补齐）。

        // ── 索引 ──
        //
        // `find_cached_task` 每次选文件都会跑，条件是
        // `file_path = ? OR file_path = ?` + `status = 'completed'` + `ORDER BY id DESC`。
        // 没有索引时这是一次全表扫描，而且因为 `id INTEGER PRIMARY KEY` 本身就是 rowid，
        // 扫描时要**往回找**最小的匹配 id——所以「命中的记录越旧，扫得越多」。
        //
        // 实测（2000 条记录、每条约 237 KB 字幕 JSON、库 464 MB）：
        //   命中最早一条：无索引 4.85 ms → 有索引 0.73 ms（6.6×）
        //   完全不存在的文件：4.19 ms → 0.21 ms（20×）
        //   命中最新一条：0.62 ms → 0.74 ms（这个场景本来就在表头，加索引无感）
        //
        // 用 (file_path, status) 复合索引：status 放进索引后，
        // `status='completed'` 这个过滤条件也不用回表。
        // IF NOT EXISTS 保证老库升级时只建一次。
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_tasks_file_path ON tasks(file_path, status);",
            [],
        )?;
        // 内容指纹索引：缓存命中走 `content_hash = ?`，与路径索引并列。
        // 老库（v0 形状）此时可能还没有 content_hash 列，直接 `CREATE INDEX`
        // 必然失败——那正是迁移（apply_migrations）要补的列。这里先探列，没有就
        // 跳过、交给迁移去建，免得每次打开老库都记一条无意义的失败告警。
        if Database::column_exists(&conn, "content_hash")? {
            conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_tasks_content_hash ON tasks(content_hash, status);",
                [],
            )?;
        }
        Ok(())
    }

    /// 把库从当前 `user_version` 逐级迁移到 [`Self::SCHEMA_VERSION`]。
    ///
    /// 每段迁移都必须**幂等**（版本号因故错位、重复执行也不出错/不重复写入）。
    ///
    /// # 为什么整段必须在一个事务里
    ///
    /// DDL 与版本号是**两次独立的写**。若不用事务，中途失败（磁盘满、库被外部
    /// 工具锁死、进程被杀）就会留下「列已加、版本没推」的半迁移状态：下次启动
    /// 重跑时 `ALTER TABLE ADD COLUMN content_hash` 会因列已存在而报错，而错误
    /// 一旦被吞掉，程序就会带着一个**版本号撒谎的库**继续跑（以为已是最新，其实
    /// 索引可能没建上）。把 `BEGIN IMMEDIATE … COMMIT` 包住「补列 + 建索引 +
    /// 推进 user_version」，任何一步失败都整体 ROLLBACK：库要么停在旧版本、
    /// 下次干净重试，要么完整升到新版本，不存在中间态。
    ///
    /// # `PRAGMA user_version` 在事务内的行为（已实测）
    ///
    /// 它写的是数据库头里的 4 字节 cookie（`OP_SetCookie` → `BTREE_USER_VERSION`），
    /// 属于**普通可回滚写**：`BEGIN IMMEDIATE` 后执行 `PRAGMA user_version = 2`，
    /// 事务内读回是 2；`ROLLBACK` 后读回是 0；`COMMIT` 之后才变 2。所以把它放进
    /// 事务里是安全的——版本推进与 DDL 严格同生共死。
    ///
    /// 用 `BEGIN IMMEDIATE`（而非 DEFERRED）：迁移必然要写，立刻拿写锁能让
    /// 「另一个实例正在写」在事务一开始就按 busy_timeout 等待/失败，而不是等
    /// 执行到某条 DDL 时才在事务中途抛错。
    fn apply_migrations(&self) -> Result<()> {
        let mut conn = self.lock_conn();
        let from: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if from >= Self::SCHEMA_VERSION {
            return Ok(());
        }

        // 事务开始后任何 `?` 提前返回都会让 `Transaction` 按 DropBehavior::Rollback
        // 回滚；只有显式 `commit()` 才会落盘。
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        // v0 → v1：历史上靠 `ALTER TABLE ADD COLUMN` 硬加出来的 metrics_json 列，
        // 现在正式纳入版本管理。老库若还没有该列就补齐（先探列，保证可重跑）。
        if from < 1 && !Database::column_exists(&tx, "metrics_json")? {
            tx.execute("ALTER TABLE tasks ADD COLUMN metrics_json TEXT", [])
                .context("v0→v1：补 metrics_json 列失败")?;
        }
        // v1 → v2：补上 `created_at` 列并回填。
        //
        // 建表语句里它是 `created_at DATETIME DEFAULT CURRENT_TIMESTAMP`，但 v0/v1
        // 形状的老库**根本没有这一列**：历史上只有 `CREATE TABLE IF NOT EXISTS` 里的
        // 定义（对已存在的表是 no-op），也从未有过 `ALTER TABLE ADD COLUMN created_at`。
        // 而查询侧 `list_recent_tasks` / `find_cached_task` / `row_to_task` 都强制
        // SELECT 它——不补这一列，老库升到 v2 后每次读历史列表都报
        // `no such column: created_at`，视频库列表与缓存命中永久失效。
        //
        // 为什么把 created_at 并入 v1→v2 而不是单开一段迁移：`SCHEMA_VERSION` 从 1 跳到 2
        // 是上一个提交刚做的，真实世界里不存在「user_version=2 且缺 created_at」的库
        //（真实老库全是 v0/v1）。单开一段 v2→v3 只会照顾这种并不存在的库；把补列并入
        // v1→v2，则 v0/v1 两种老库一步到位。content_hash 另有一段 v2→v3（见下）。
        //
        // 默认值为什么不是 `CURRENT_TIMESTAMP`：`ALTER TABLE ADD COLUMN` **不允许
        // 非常量默认值**——表里已有行时 `DEFAULT CURRENT_TIMESTAMP` 与
        // `DEFAULT (datetime('now'))` 都会直接报 "Cannot add a column with
        // non-constant default"（SQLite 3.45 实测；只有空表才碰巧放过）。所以先加
        // 常量 `DEFAULT ''`，再对老行回填一个真实时间戳，让日期切片有 10 个字符可切。
        // 新建的库/新建的行仍走建表默认值（`insert_task` 亦显式给值）。
        if from < 2 {
            if !Database::column_exists(&tx, "created_at")? {
                tx.execute(
                    "ALTER TABLE tasks ADD COLUMN created_at DATETIME DEFAULT ''",
                    [],
                )
                .context("v1→v2：补 created_at 列失败")?;
            }
            // 幂等回填：只补「NULL / 空串」的行。`''` 不可能与 `CURRENT_TIMESTAMP`
            // 产出的 `YYYY-MM-DD HH:MM:SS` 相撞，因此重跑不会覆盖真实时间戳
            // （真实磁盘库 `voice2word.db` 的 created_at 就是有值的）。
            let backfilled_at = utc_now();
            tx.execute(
                "UPDATE tasks SET created_at = ?1 WHERE created_at IS NULL OR created_at = ''",
                params![backfilled_at],
            )
            .context("v1→v2：回填 created_at 失败")?;
        }
        // v2 → v3：加入内容指纹列与索引。
        if from < 3 {
            if !Database::column_exists(&tx, "content_hash")? {
                tx.execute("ALTER TABLE tasks ADD COLUMN content_hash TEXT", [])
                    .context("v2→v3：补 content_hash 列失败")?;
            }
            // 索引带 IF NOT EXISTS，重跑安全；若同名对象已被别的东西（表/视图）占用，
            // 这里会如实报错并触发整体回滚，而不是留下「列在、索引不在」的残局。
            tx.execute(
                "CREATE INDEX IF NOT EXISTS idx_tasks_content_hash ON tasks(content_hash, status);",
                [],
            )
            .context("v2→v3：创建 idx_tasks_content_hash 失败")?;
        }
        // v3 → v4：把历史列表页的三个派生字段物化成列，读侧不再解析 JSON。
        //
        // 旧的 `list_recent_tasks` 每次都用 `json_each` 把 `segments_json` 逐句
        // 展开算「有没有译文」、用 `json_extract` 取首句字段——50 条几百句的
        // 工程就是几 MB JSON，且全发生在 UI 线程。写入侧本来就拿着 `&[Segment]`，
        // 把摘要顺手落列是零成本；读侧于是变成纯 SELECT。
        //
        // 下面这条 UPDATE 是这三个新列**唯一**允许解析 JSON 的地方（每个库只跑
        // 一次，且在迁移事务里）：口径与旧 SQL 逐字一致——`polished` 判空必须
        // 显式列出 ASCII 空白字符集，SQLite 默认的 `TRIM(X)` 只去半角空格，
        // `"  \n "` 会被误当成有效润色稿。
        //
        // 不做「只更新默认值的行」之类的条件：这三列是 `segments_json` 的派生
        // 数据，重算结果与旧口径一致，重跑迁移也不会造出不一致。
        if from < 4 {
            for (name, ddl) in [
                (
                    "segment_count",
                    "ALTER TABLE tasks ADD COLUMN segment_count INTEGER NOT NULL DEFAULT 0",
                ),
                (
                    "sample_text",
                    "ALTER TABLE tasks ADD COLUMN sample_text TEXT NOT NULL DEFAULT ''",
                ),
                (
                    "has_translation",
                    "ALTER TABLE tasks ADD COLUMN has_translation INTEGER NOT NULL DEFAULT 0",
                ),
            ] {
                if !Database::column_exists(&tx, name)? {
                    tx.execute(ddl, [])
                        .with_context(|| format!("v3→v4：补 {name} 列失败"))?;
                }
            }
            let backfill = format!(
                "UPDATE tasks SET
                     segment_count = CASE WHEN json_valid(segments_json)
                                          THEN COALESCE(json_array_length(segments_json), 0) ELSE 0 END,
                     sample_text = CASE WHEN json_valid(segments_json)
                                        THEN COALESCE(NULLIF(TRIM(json_extract(segments_json, '$[0].polished'),
                                                                 {ascii_ws}), ''),
                                                     json_extract(segments_json, '$[0].text'), '') ELSE '' END,
                     has_translation = CASE WHEN json_valid(segments_json)
                                            THEN CASE WHEN EXISTS (
                                                     SELECT 1 FROM json_each(segments_json) AS seg
                                                     WHERE TRIM(COALESCE(json_extract(seg.value, '$.translation'), ''),
                                                                {unicode_ws}) <> ''
                                                 ) THEN 1 ELSE 0 END
                                            ELSE 0 END",
                ascii_ws = SQL_ASCII_WHITESPACE_SET,
                unicode_ws = SQL_UNICODE_WHITESPACE_SET,
            );
            tx.execute(&backfill, [])
                .context("v3→v4：回填列表摘要列失败")?;
        }

        // 版本号与上面的 DDL 在同一个事务里提交：要么一起生效，要么一起回滚。
        tx.pragma_update(None, "user_version", Self::SCHEMA_VERSION)
            .with_context(|| format!("写入 user_version={} 失败", Self::SCHEMA_VERSION))?;
        tx.commit().context("提交 schema 迁移事务失败")?;

        info!(from, to = Self::SCHEMA_VERSION, "数据库 schema 已迁移");
        Ok(())
    }

    pub fn insert_task(
        &self,
        file_path: &str,
        file_name: &str,
        duration: f64,
        status: &str,
        segments: &[Segment],
        metrics: Option<&crate::core::PipelinePerformanceMetrics>,
    ) -> Result<i64> {
        let conn = self.lock_conn();
        let segments_json = serde_json::to_string(segments).context("序列化 segments 失败")?;
        let metrics_json = metrics.and_then(|m| serde_json::to_string(m).ok());
        // 摘要列与 segments_json 在**同一条 INSERT**里落库：一旦拆成两次写，
        // 中间崩溃/失败就会留下「列说 N 句、JSON 里却是别的句数」的永久错位，
        // 而读侧（`list_recent_tasks`）已经不再回头解析 JSON。
        let (segment_count, sample_text, has_translation) = summarize_segments(segments);
        // 落库时顺手算一次内容指纹（采样哈希，毫秒级）。失败（文件已删/不可读）
        // 就存 NULL，缓存回退到「只按路径」的老行为。
        let content_hash = crate::utils::fingerprint::media_fingerprint(file_path);

        // 重试只针对「库被占用」这类瞬时错误。SQLite 在返回 BUSY 时保证语句
        // **完全未执行**，因此重试不带副作用（INSERT 要么没执行，要么整条成功）。
        //
        // 显式写入 `created_at`（UTC，与建表默认值同形状）：列上的 DEFAULT 只在
        // 「INSERT 未提及该列」时生效，而历史库的该列是迁移加出来的
        // `DEFAULT ''`（见 `apply_migrations`）——不显式给值，新行会落成空串，
        // 界面日期列就会一直空白。
        let created_at = utc_now();
        with_busy_retry(|| {
            conn.execute(
                "INSERT INTO tasks (file_path, file_name, duration, status, segments_json, metrics_json, content_hash, created_at, segment_count, sample_text, has_translation) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![file_path, file_name, duration, status, segments_json, metrics_json, content_hash, created_at, segment_count as i64, sample_text, has_translation],
            )
        })?;

        Ok(conn.last_insert_rowid())
    }

    /// 列出最近任务（**不装载字幕正文、不解析 JSON**）。
    ///
    /// 句数 / 首句摘录 / 翻译状态在**写入侧**（`insert_task` /
    /// `update_task_segments`）随 `segments_json` 一起物化进 `segment_count` /
    /// `sample_text` / `has_translation` 三列，这里只做纯 SELECT：旧实现用
    /// `json_each` 逐句展开 `segments_json`、用 `json_extract` 取首句字段，
    /// 一条几百句的工程每次列表查询都要解析数百 KB JSON——库越大越明显，
    /// 而且这段开销全在 UI 线程上。老库由 `apply_migrations` 的 v3→v4 段
    /// 一次性回填，回填口径即旧的 SQL 口径（`polished` 需按 **ASCII 空白集**
    /// 判空后取值，不能用 SQLite 默认只去半角空格的 `TRIM(X)`）。
    pub fn list_recent_tasks(&self, limit: usize) -> Result<Vec<TaskRecord>> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(
            "SELECT id, file_path, file_name, duration, status, created_at, metrics_json, content_hash,
                    segment_count, sample_text, has_translation
             FROM tasks ORDER BY id DESC LIMIT ?1",
        )?;

        let rows = stmt.query_map([limit], |row| {
            let metrics_json: Option<String> = row.get(6).unwrap_or(None);
            let metrics: Option<crate::core::PipelinePerformanceMetrics> = metrics_json
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok());
            Ok(TaskRecord {
                id: row.get(0)?,
                file_path: row.get(1)?,
                file_name: row.get(2)?,
                // `duration` / `created_at` 都是**可空列**（`REAL DEFAULT 0.0` /
                // `DATETIME DEFAULT CURRENT_TIMESTAMP`），而 DEFAULT 只在 INSERT
                // 未提及该列时生效——显式写 NULL 照存，且没有任何迁移回填 duration。
                // 老库/外部工具留下的 NULL 行若按必填读，`?` 会把**整张历史列表**
                // 拖成 Err；调用方再 `unwrap_or_default()`，用户看到的是「视频库整个
                // 空了」，而不是一条记录降级。与相邻的 segment_count / sample_text /
                // content_hash 一样，按缺省值容错读。
                duration: row.get::<_, Option<f64>>(3)?.unwrap_or(0.0),
                status: row.get(4)?,
                created_at: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
                metrics,
                segments: Vec::new(),
                segment_count: row.get::<_, i64>(8).unwrap_or(0).max(0) as usize,
                sample_text: row.get(9).unwrap_or_default(),
                // 第 11 列：写入侧物化的「至少一句带非空译文」标志（见字段
                // 说明），空壳记录也据此正确回答翻译状态；读失败按「未翻译」
                // 容错，一行坏数据不能拖垮整个列表。
                has_translation: row.get::<_, i64>(10).unwrap_or(0) != 0,
                content_hash: row.get(7).unwrap_or(None),
                segments_loaded: false,
            })
        })?;

        let mut tasks = Vec::new();
        for r in rows {
            tasks.push(r?);
        }
        Ok(tasks)
    }

    /// 按 id 装载某条工程的完整字幕（仅在真正需要正文时调用）。
    ///
    /// 坏 `segments_json`（历史版本、手动改库、外部工具、磁盘损坏）**返回 `Err`**
    /// 并带上 task id，而不是像旧实现那样 `.ok()` + `unwrap_or_default()` 静默变成
    /// 空表——那会让用户以为「这份工程没有字幕」。实例：`ui/actions.rs` 的批量导出
    /// 已按 `Err` 分支提示「{name}（读取失败: {e}）」，这条路径因此能报出真因。
    ///
    /// 行不存在 / 列为 NULL / 空串仍是空表 + `Ok`：那是「确实没有字幕」，不是坏数据。
    pub fn load_task_segments(&self, id: i64) -> Result<Vec<Segment>> {
        let report = self.load_task_segments_report(id)?;
        match report.decode_error {
            Some(err) => Err(anyhow::Error::new(err).context("装载历史工程字幕失败")),
            None => Ok(report.segments),
        }
    }

    /// 同 [`Database::load_task_segments`]，但把「坏 `segments_json`」降级进返回值，
    /// 而不是 `Err`。
    ///
    /// 给那些签名不能改成 `Result` 的调用方用（例如 `AppState::load_task_with` 那条
    /// 直接把结果灌进 `self.segments` 的装载路径）：它拿到的仍是空表，但可以顺手把
    /// [`SegmentLoadReport::decode_error`] 挂到界面上，而不是让用户面对一个什么都
    /// 没有的剪辑台。真正的数据库错误（加锁失败、SQL 失败）依旧是 `Err`。
    pub fn load_task_segments_report(&self, id: i64) -> Result<SegmentLoadReport> {
        let conn = self.lock_conn();
        let json: Option<String> = conn
            .query_row(
                "SELECT segments_json FROM tasks WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()?;
        let (mut segments, decode_error) = decode_segments_json(id, json.as_deref());
        normalize_segment_blanks(&mut segments);
        Ok(SegmentLoadReport {
            segments,
            decode_error,
        })
    }

    /// 把一条列表记录补齐为「已装载字幕」的完整记录。
    ///
    /// **新的调用点请用 [`Database::try_hydrate_task`]**：本方法签名不能返回 `Result`
    /// （`AppState::load_task_with` 直接吃 `TaskRecord`，改签名会波及调用方文件），
    /// 所以装载失败时只能降级——保留列表里由写入侧物化的摘要，并留一发带 task id 的
    /// `warn!`，至少不再是「静默变成空字幕」。
    pub fn hydrate_task(&self, task: &TaskRecord) -> TaskRecord {
        match self.try_hydrate_task(task) {
            Ok(hydrated) => hydrated,
            Err(err) => {
                warn!(
                    task_id = task.id,
                    error = %err,
                    "载入历史工程字幕失败：按未装载处理，保留列表里的句数与摘录"
                );
                // 保留 `segment_count` / `sample_text`（视频库卡片仍能显示「300 句 /
                // 首句」），并明确标回 `segments_loaded = false`：一次失败的装载不能
                // 伪装成「装载成功且字幕为空」。
                task.clone()
            }
        }
    }

    /// 与 [`Database::hydrate_task`] 同源，但把装载失败**作为 `Err` 返回**。
    ///
    /// 推荐接法（`AppState::load_task_with`）：
    ///
    /// ```ignore
    /// let task = match self.db.try_hydrate_task(task) {
    ///     Ok(task) => task,
    ///     Err(err) => {
    ///         // 用户可见：一次性提示（例如 ProcessStatus::Failed(err.to_string())），
    ///         // 并且**不要**清空当前工程 / 不要切到剪辑台
    ///         tracing::warn!(error = %err, "载入历史工程失败");
    ///         return;
    ///     }
    /// };
    /// ```
    pub fn try_hydrate_task(&self, task: &TaskRecord) -> Result<TaskRecord> {
        if task.segments_loaded {
            return Ok(task.clone());
        }
        let report = self.load_task_segments_report(task.id)?;
        if let Some(err) = report.decode_error {
            return Err(anyhow::Error::new(err).context(format!(
                "载入工程「{}」（id={}）的字幕失败",
                task.file_name, task.id
            )));
        }
        let mut out = task.clone();
        out.segment_count = report.segments.len();
        out.segments = report.segments;
        out.segments_loaded = true;
        // 正文刚装载回来：翻译状态按同一口径重算（列表空壳里的标志来自
        // 写入侧物化的列，装载后以内存字幕为准）。
        out.has_translation = out.segments.iter().any(|segment| segment.has_translation());
        Ok(out)
    }

    /// 更新指定任务（按主键 `id`）的字幕片段列表 (用于在编辑器中修改错字或时间后写回)
    ///
    /// **必须按 `id` 定位，不能按 `file_path`**：同一个路径允许存在多条历史记录——
    /// 每完成一次转写就 `insert_task` 一行（重转写、换参数重跑都会生成新行），
    /// 0 秒缓存命中走的也是 `ORDER BY id DESC LIMIT 1` 取最新一条。若按路径批量更新，
    /// 用户在剪辑台里对当前工程的任何修改都会被同时灌进同一路径的所有历史记录，
    /// 表现为「编辑最新一条，另一条老记录的字幕/译文也跟着被覆盖」。
    pub fn update_task_segments(&self, id: i64, segments: &[Segment]) -> Result<()> {
        let conn = self.lock_conn();
        let segments_json = serde_json::to_string(segments).context("序列化 segments 失败")?;
        let total_dur = segments.last().map(|s| s.end).unwrap_or(0.0);
        // 摘要列与 `segments_json` 在同一条 UPDATE 里刷新，理由同 `insert_task`：
        // 两处一旦分开写，中途失败就会留下与正文不一致的摘要素材。
        let (segment_count, sample_text, has_translation) = summarize_segments(segments);
        with_busy_retry(|| {
            conn.execute(
                // `MAX(NULL, ?2)` 在 SQLite 里仍是 NULL（标量 max() 遇 NULL 即 NULL），
                // 于是「编辑并保存」这条唯一的自然修复路径永远修不好 NULL duration。
                // COALESCE 把 NULL 当 0 参与取大：既保留「duration 只增不减」的既有
                // 口径（见 `state.rs` 载入时的 `task.duration.max(timeline_end)`），
                // 又让编辑能把坏行救回来。
                "UPDATE tasks SET segments_json = ?1, duration = MAX(COALESCE(duration, 0.0), ?2),
                        segment_count = ?3, sample_text = ?4, has_translation = ?5
                 WHERE id = ?6",
                params![
                    segments_json,
                    total_dur,
                    segment_count as i64,
                    sample_text,
                    has_translation,
                    id
                ],
            )
        })?;
        Ok(())
    }

    /// 与 [`Database::update_task_segments`] 同源，但直接接收**已经序列化好的**
    /// `segments_json`。
    ///
    /// 给后台写库线程用：那边手上只有序列化完成的 JSON（序列化发生在渲染线程之外），
    /// 不该为了写库再把整个数组解析成 `Vec<Segment>` 又序列化回去。派生列与 `duration`
    /// 的刷新口径必须与 `update_task_segments` **完全一致**，所以这里解析一次只为算摘要，
    /// 真正写回的字节就是调用方给的那一份（不做二次序列化，避免格式漂移）。
    ///
    /// 坏 JSON 返回 `Err`（带 task id）：损坏数据绝不能覆盖库里已有的字幕。
    pub fn update_task_segments_json(&self, id: i64, segments_json: &str) -> Result<()> {
        let segments: Vec<Segment> = serde_json::from_str(segments_json)
            .with_context(|| format!("待写库的 segments_json 无法解析（task_id={id}）"))?;
        let conn = self.lock_conn();
        let total_dur = segments.last().map(|s| s.end).unwrap_or(0.0);
        let (segment_count, sample_text, has_translation) = summarize_segments(&segments);
        with_busy_retry(|| {
            conn.execute(
                "UPDATE tasks SET segments_json = ?1, duration = MAX(COALESCE(duration, 0.0), ?2),
                        segment_count = ?3, sample_text = ?4, has_translation = ?5
                 WHERE id = ?6",
                params![
                    segments_json,
                    total_dur,
                    segment_count as i64,
                    sample_text,
                    has_translation,
                    id
                ],
            )
        })?;
        Ok(())
    }

    /// 查找指定文件的历史已完成转写记录（0 秒智能缓存命中）。
    ///
    /// # 命中优先级：内容指纹 > 路径
    ///
    /// 先用**内容指纹**查：同一份内容即使改名/换目录也能命中。找不到再退回
    /// **路径**匹配，但只认 `content_hash IS NULL` 的老记录——若路径相同、指纹
    /// 不同，说明这份视频已被覆盖成别的内容，**绝不能**返回旧字幕（那是静默的
    /// 正确性错误）。文件读不到（指纹为 `None`）时退回纯路径匹配，与老行为一致。
    pub fn find_cached_task(&self, file_path: &str) -> Result<Option<TaskRecord>> {
        let conn = self.lock_conn();
        let fp = crate::utils::fingerprint::media_fingerprint(file_path);

        if let Some(ref fp) = fp {
            let mut stmt = conn.prepare(
                "SELECT id, file_path, file_name, duration, status, segments_json, created_at, metrics_json, content_hash
                 FROM tasks
                 WHERE content_hash = ?1 AND status = 'completed'
                 ORDER BY id DESC LIMIT 1",
            )?;
            let mut rows = stmt.query_map(params![fp], Self::row_to_task)?;
            if let Some(r) = rows.next() {
                let record = r?;
                if !record.segments.is_empty() {
                    return Ok(Some(record));
                }
            }
        }

        // 路径回退：仅在「无指纹」或「老记录（content_hash 为 NULL）」时启用，
        // 避免同路径换内容时错误命中。
        let normalized_1 = file_path.replace('\\', "/");
        let normalized_2 = file_path.replace('/', "\\");
        let path_only_legacy = fp.is_none();
        let sql = if path_only_legacy {
            "SELECT id, file_path, file_name, duration, status, segments_json, created_at, metrics_json, content_hash
             FROM tasks
             WHERE (file_path = ?1 OR file_path = ?2) AND status = 'completed'
             ORDER BY id DESC LIMIT 1"
        } else {
            "SELECT id, file_path, file_name, duration, status, segments_json, created_at, metrics_json, content_hash
             FROM tasks
             WHERE (file_path = ?1 OR file_path = ?2) AND status = 'completed' AND content_hash IS NULL
             ORDER BY id DESC LIMIT 1"
        };
        let mut stmt = conn.prepare(sql)?;
        let mut rows = stmt.query_map(params![normalized_1, normalized_2], Self::row_to_task)?;
        if let Some(r) = rows.next() {
            let record = r?;
            if !record.segments.is_empty() {
                return Ok(Some(record));
            }
        }
        Ok(None)
    }

    /// 把一行（列顺序：id, file_path, file_name, duration, status, segments_json,
    /// created_at, metrics_json, content_hash）转成 `TaskRecord`。
    fn row_to_task(row: &rusqlite::Row) -> rusqlite::Result<TaskRecord> {
        let segments_json: String = row.get(5)?;
        // 坏 JSON 仍按空表降级：`find_cached_task` 的调用方是 `.ok().flatten()`，
        // 这里返回 Err 只会被吞成「未命中」，让用户把已经转写过的文件重转一遍。
        // 但必须留下可读线索——旧实现连一行日志都没有，用户只看到「字幕空了」。
        let mut segments: Vec<Segment> = match serde_json::from_str(&segments_json) {
            Ok(segments) => segments,
            Err(err) => {
                warn!(
                    task_id = row.get::<_, i64>(0)?,
                    raw_len = segments_json.len(),
                    error = %err,
                    "历史库 segments_json 解析失败：该记录按无字幕处理（缓存查询会视为未命中）"
                );
                Vec::new()
            }
        };
        // 反序列化即归一：`sample_text` 与写回路径共用这份内存态，
        // 让「全空白」在内存里就变成 None / 空串。
        normalize_segment_blanks(&mut segments);
        let metrics_json: Option<String> = row.get(7).unwrap_or(None);
        let metrics: Option<crate::core::PipelinePerformanceMetrics> = metrics_json
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok());
        let sample_text = segments
            .first()
            .map(|s| s.display_text().to_string())
            .unwrap_or_default();
        // 与 `list_recent_tasks` 第 11 列同一口径：任一句带非空译文即 true。
        // 必须在 `segments` 被 move 进结构体之前算好。
        let has_translation = segments.iter().any(|s| s.has_translation());
        Ok(TaskRecord {
            id: row.get(0)?,
            file_path: row.get(1)?,
            file_name: row.get(2)?,
            // 同 `list_recent_tasks`：可空列按缺省值容错读，一行 NULL 不能把
            // `find_cached_task` 的整次查询变成 Err（那会被调用方 `.ok()` 吞成
            // 「未命中」，用户被迫把已经转写过的文件重转一遍）。
            duration: row.get::<_, Option<f64>>(3)?.unwrap_or(0.0),
            status: row.get(4)?,
            segment_count: segments.len(),
            sample_text,
            segments_loaded: true,
            has_translation,
            segments,
            created_at: row.get::<_, Option<String>>(6)?.unwrap_or_default(),
            content_hash: row.get(8).unwrap_or(None),
            metrics,
        })
    }

    /// 收尾维护：`PRAGMA optimize` + WAL checkpoint(TRUNCATE)。
    ///
    /// WAL 模式下每次写入都追加进 `-wal`，只在 checkpoint 时才并回主库；进程
    /// 正常退出若从不 checkpoint，`-wal` 会持续增长（大库能到几十 MB），下次
    /// 冷启动要重放整个 WAL 才可用。关程序时做一次 TRUNCATE checkpoint：
    /// - `PRAGMA optimize`：让 SQLite 按查询统计更新索引/表统计（它自己决定是否需要
    ///   ANALYZE），比无条件 ANALYZE 便宜、也足以让查询计划长期保持在线；
    /// - `wal_checkpoint(TRUNCATE)`：把 WAL 全部并回主库并把文件截断到 0。
    ///
    /// 两个操作都**尽力而为**：拿不到锁 / 被占用时只记日志、绝不阻断退出。
    pub fn maintain_on_shutdown(&self) {
        // 与其它方法统一走 `lock_conn`：中毒容忍只此一处实现，不留两套风格。
        let conn = self.lock_conn();
        if let Err(err) = conn.execute_batch("PRAGMA optimize;") {
            warn!(error = %err, "退出维护 PRAGMA optimize 失败（忽略）");
        }
        match conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
            ))
        }) {
            Ok((busy, log_pages, checkpointed)) => {
                if busy != 0 {
                    info!(
                        log_pages,
                        checkpointed, "退出 checkpoint：库仍被占用，已尽量合并（下次启动继续）"
                    );
                } else {
                    info!(
                        log_pages,
                        checkpointed, "退出 checkpoint：WAL 已并回主库并截断"
                    );
                }
            }
            Err(err) => warn!(error = %err, "退出 checkpoint 失败（忽略，下次启动会继续）"),
        }
    }

    /// 删除指定历史任务记录
    pub fn delete_task(&self, id: i64) -> Result<()> {
        let conn = self.lock_conn();
        with_busy_retry(|| conn.execute("DELETE FROM tasks WHERE id = ?1", params![id]))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(text: &str) -> Vec<Segment> {
        vec![Segment::new(1, 0.0, 1.0, text)]
    }

    /// 同一路径允许多条历史记录（每次转写各插一行）。写回编辑结果只能命中
    /// 目标那一条，否则用户在剪辑台改一个字，同路径的另一条老记录也会被覆盖。
    #[test]
    fn update_by_id_only_touches_target_row() {
        let db = Database::open(":memory:").expect("内存数据库应能打开");
        let path = "D:/video/a.mp4";
        let first = db
            .insert_task(path, "a.mp4", 1.0, "completed", &seg("第一次转写"), None)
            .unwrap();
        let second = db
            .insert_task(path, "a.mp4", 1.0, "completed", &seg("第二次转写"), None)
            .unwrap();
        assert_ne!(first, second);

        // 编辑「第一条」记录
        db.update_task_segments(first, &seg("改过的第一句"))
            .unwrap();

        let loaded_first = db.load_task_segments(first).unwrap();
        let loaded_second = db.load_task_segments(second).unwrap();
        assert_eq!(loaded_first[0].text, "改过的第一句");
        assert_eq!(
            loaded_second[0].text, "第二次转写",
            "按 id 更新不应波及同路径的另一条记录"
        );
    }
    /// 译文与**目标语言**都必须能穿过 SQLite 往返存活。
    ///
    /// 这是本次翻译改造的关键依赖：`translation_lang` 一旦在落库/装载时丢失，
    /// 重启后所有句子都会被判为「非当前目标语言」，触发整篇重译（在线接口直接烧钱），
    /// 而且用户会看到「明明译过却又要重译」。
    #[test]
    fn translation_language_survives_db_roundtrip() {
        let db = Database::open(":memory:").expect("内存数据库应能打开");

        let mut a = Segment::new(1, 0.0, 1.0, "你好");
        a.translation = Some("Hello".to_string());
        a.translation_lang = Some("English".to_string());

        let mut b = Segment::new(2, 1.0, 2.0, "世界");
        b.translation = Some("Bonjour".to_string());
        b.translation_lang = Some("Français".to_string());

        // 第三句故意没有译文，验证 None 也能往返
        let c = Segment::new(3, 2.0, 3.0, "再见");

        let id = db
            .insert_task("D:/v.mp4", "v.mp4", 3.0, "completed", &[a, b, c], None)
            .unwrap();

        let loaded = db.load_task_segments(id).unwrap();
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded[0].translation.as_deref(), Some("Hello"));
        assert_eq!(loaded[0].translation_lang.as_deref(), Some("English"));
        assert_eq!(loaded[1].translation_lang.as_deref(), Some("Français"));
        assert!(loaded[2].translation.is_none());
        assert!(loaded[2].translation_lang.is_none());

        // 判定仍然正确：只有英文那句算「已是英文」
        assert!(loaded[0].translation_matches("English"));
        assert!(!loaded[1].translation_matches("English"));
        assert_eq!(
            loaded
                .iter()
                .filter(|s| s.translation_matches("English"))
                .count(),
            1
        );
    }

    /// P0-7：同一份内容、不同路径（复制/改名）应命中同一条缓存。
    #[test]
    fn content_hash_cache_hits_across_paths() {
        let dir = std::env::temp_dir().join(format!("v2w_fp_cache_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("original.mp4");
        let b = dir.join("copy.mp4");
        std::fs::write(&a, vec![3u8; 2048]).unwrap();
        std::fs::write(&b, vec![3u8; 2048]).unwrap();

        let db = Database::open(":memory:").unwrap();
        db.insert_task(
            &a.to_string_lossy(),
            "original.mp4",
            1.0,
            "completed",
            &seg("内容一致"),
            None,
        )
        .unwrap();

        // 用另一个路径查同一内容 → 必须命中
        let hit = db.find_cached_task(&b.to_string_lossy()).unwrap();
        assert!(hit.is_some(), "同内容不同路径应命中缓存");
        assert_eq!(hit.unwrap().segments[0].text, "内容一致");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P0-7：路径不变但**内容已变**（重新导出覆盖同名文件）时，绝不能返回旧字幕。
    /// 这是旧的「只按路径命中」会犯的静默正确性错误。
    #[test]
    fn content_hash_cache_misses_when_file_changed_in_place() {
        let dir = std::env::temp_dir().join(format!("v2w_fp_changed_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("v.mp4");
        std::fs::write(&a, vec![1u8; 2048]).unwrap();
        let db = Database::open(":memory:").unwrap();
        db.insert_task(
            &a.to_string_lossy(),
            "v.mp4",
            1.0,
            "completed",
            &seg("旧字幕"),
            None,
        )
        .unwrap();

        // 覆盖成不同内容（大小也不同）
        std::fs::write(&a, vec![9u8; 4096]).unwrap();
        let hit = db.find_cached_task(&a.to_string_lossy()).unwrap();
        assert!(hit.is_none(), "同路径内容已变时不应命中旧缓存");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P0-5：新库打开后 `user_version` 必须被推进到当前 schema 版本，
    /// 否则「版本化迁移」形同虚设。
    #[test]
    fn fresh_db_records_schema_version() {
        let db = Database::open(":memory:").expect("内存数据库应能打开");
        let v: i32 = db
            .lock_conn()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, Database::SCHEMA_VERSION);
    }

    /// P0-5：老库（user_version=0、缺 metrics_json 列）打开后应被迁移到最新版本，
    /// 且原有数据不丢。
    #[test]
    fn legacy_db_is_migrated_without_data_loss() {
        let dir = std::env::temp_dir().join(format!("v2w_mig_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("legacy.db");

        // 造一个「旧 schema」库：没有 metrics_json 列、user_version 保持 0
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute(
                "CREATE TABLE tasks (id INTEGER PRIMARY KEY AUTOINCREMENT, file_path TEXT NOT NULL, \
                 file_name TEXT NOT NULL, duration REAL DEFAULT 0.0, status TEXT NOT NULL, \
                 segments_json TEXT NOT NULL, created_at DATETIME DEFAULT CURRENT_TIMESTAMP);",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO tasks (file_path, file_name, duration, status, segments_json) \
                 VALUES ('D:/a.mp4','a.mp4',1.0,'completed','[]')",
                [],
            )
            .unwrap();
            let v: i32 = conn
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap();
            assert_eq!(v, 0, "前置：旧库版本应为 0");
        }

        let db = Database::open(&path).expect("老库应能被迁移后打开");
        let v: i32 = db
            .lock_conn()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, Database::SCHEMA_VERSION, "迁移后版本号应推进");
        // 老数据还在，且 metrics_json 列已补齐（可正常 SELECT）
        let n: i64 = db
            .lock_conn()
            .query_row("SELECT count(*) FROM tasks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "迁移不应丢数据");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P0-5：库文件损坏时，`open` 不能把整个程序拖死——应改名留底并新建空库。
    #[test]
    fn corrupt_db_is_quarantined_and_reopened_empty() {
        let dir = std::env::temp_dir().join(format!("v2w_corrupt_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("broken.db");
        // 写一段「像 SQLite 但头被破坏」的字节：SQLite 头魔数 + 垃圾
        let mut bytes = b"SQLite format 3\0".to_vec();
        bytes.extend_from_slice(&[0xFFu8; 4096]);
        std::fs::write(&path, &bytes).unwrap();

        let db = Database::open(&path).expect("损坏库应被自愈后仍能打开");
        // 新库是空的
        let n: i64 = db
            .lock_conn()
            .query_row("SELECT count(*) FROM tasks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
        // 原损坏文件被改名留底（目录里应存在一个 .corrupt- 备份）
        let has_backup = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().contains(".corrupt-"));
        assert!(has_backup, "损坏库应被改名留底，而不是删除");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 迁移必须整体成对完成：迁移后版本号推进，**且**新旧字段都能正常读写。
    #[test]
    fn migration_adds_fingerprint_column_writable() {
        let dir = std::env::temp_dir().join(format!("v2w_mig2_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("legacy.db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute(
                "CREATE TABLE tasks (id INTEGER PRIMARY KEY AUTOINCREMENT, file_path TEXT NOT NULL, \
                 file_name TEXT NOT NULL, duration REAL DEFAULT 0.0, status TEXT NOT NULL, \
                 segments_json TEXT NOT NULL, metrics_json TEXT, \
                 created_at DATETIME DEFAULT CURRENT_TIMESTAMP);",
                [],
            )
            .unwrap();
            // 前置：老库没有 content_hash 列
            assert!(conn
                .prepare("SELECT content_hash FROM tasks LIMIT 1")
                .is_err());
        }

        let db = Database::open(&path).expect("老库应能被迁移后打开");
        // 迁移后 content_hash 列可用：插入与查询都应工作
        let id = db
            .insert_task("D:/x.mp4", "x.mp4", 1.0, "completed", &seg("迁移后"), None)
            .unwrap();
        let loaded = db.load_task_segments(id).unwrap();
        assert_eq!(loaded[0].text, "迁移后");
        let n: i64 = db
            .lock_conn()
            .query_row(
                "SELECT count(*) FROM tasks WHERE content_hash IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        // 文件不存在 → 指纹为 NULL，这本身正常；这里只要求该列可被查询而不再报错。
        assert_eq!(n, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 回归：`user_version=2` 但缺 `content_hash` 的库必须被 v2→v3 迁移补齐。
    ///
    /// 这正是本轮修掉的 bug：`SCHEMA_VERSION` 曾停在 2，而 `apply_migrations` 里
    /// 已存在 v2→v3 段，于是 `from >= SCHEMA_VERSION` 让这类库**永远跳过**该段——
    /// 列与索引永久缺失，缓存命中查询直接 `no such column: content_hash`。修法是让
    /// 版本号与迁移段一致（推进到 3），本用例即锁死这一行为。
    #[test]
    fn v2_db_without_fingerprint_upgrades_to_v3() {
        let dir = tmp_dir("v2_to_v3");
        let path = dir.join("legacy_v2.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE tasks (
                     id INTEGER PRIMARY KEY AUTOINCREMENT,
                     file_path TEXT NOT NULL,
                     file_name TEXT NOT NULL,
                     duration REAL DEFAULT 0.0,
                     status TEXT NOT NULL,
                     segments_json TEXT NOT NULL,
                     metrics_json TEXT,
                     created_at DATETIME DEFAULT CURRENT_TIMESTAMP
                 );
                 INSERT INTO tasks (file_path, file_name, duration, status, segments_json, created_at)
                     VALUES ('D:/v2/a.mp4', 'a.mp4', 1.0, 'completed', '[]', '2026-09-19 02:58:21');
                 PRAGMA user_version = 2;",
            )
            .unwrap();
            // 前置：这是 v2 且缺 content_hash，正是旧版本号会让迁移跳过的形状
            assert!(
                !Database::column_exists(&conn, "content_hash").unwrap(),
                "前置条件：v2 库不应有 content_hash 列"
            );
        }

        let db = Database::open(&path).expect("v2 库应能迁移后打开");
        assert_eq!(
            user_version(&db),
            Database::SCHEMA_VERSION,
            "版本号应推进到当前版本"
        );
        {
            let conn = db.lock_conn();
            assert!(
                Database::column_exists(&conn, "content_hash").unwrap(),
                "v2→v3 应补齐 content_hash 列"
            );
        }
        assert!(
            index_exists(&db, "idx_tasks_content_hash"),
            "v2→v3 应建上内容指纹索引"
        );
        // 迁移不改数据，且新列可写
        let id = db
            .insert_task(
                "D:/v2/b.mp4",
                "b.mp4",
                1.0,
                "completed",
                &seg("v3 后"),
                None,
            )
            .unwrap();
        assert_eq!(db.load_task_segments(id).unwrap()[0].text, "v3 后");
        let n: i64 = db
            .lock_conn()
            .query_row("SELECT count(*) FROM tasks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2, "迁移不应丢数据");
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }
    /// 「库被占用」是唯一允许重试的错误类别，其余错误必须立刻返回，
    /// 否则一次约束冲突会被拖成好几秒的假死。
    #[test]
    fn busy_retry_only_retries_lock_errors() {
        use std::cell::Cell;
        // 1) 非 busy 错误：只调用一次，不重试
        let calls = Cell::new(0);
        let res: rusqlite::Result<()> = with_busy_retry(|| {
            calls.set(calls.get() + 1);
            Err(rusqlite::Error::QueryReturnedNoRows)
        });
        assert!(res.is_err());
        assert_eq!(calls.get(), 1, "非占用错误不应重试");

        // 2) busy 错误：重试到上限后放弃（次数 = BUSY_RETRY_ATTEMPTS）
        let calls = Cell::new(0);
        let res: rusqlite::Result<()> = with_busy_retry(|| {
            calls.set(calls.get() + 1);
            Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error {
                    code: ErrorCode::DatabaseBusy,
                    extended_code: 5,
                },
                None,
            ))
        });
        assert!(res.is_err());
        assert_eq!(calls.get(), BUSY_RETRY_ATTEMPTS, "占用错误应重试到上限");

        // 3) 先占用后成功：最终返回成功
        let calls = Cell::new(0);
        let res: rusqlite::Result<u32> = with_busy_retry(|| {
            calls.set(calls.get() + 1);
            if calls.get() == 1 {
                Err(rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error {
                        code: ErrorCode::DatabaseLocked,
                        extended_code: 6,
                    },
                    None,
                ))
            } else {
                Ok(42)
            }
        });
        assert_eq!(res.unwrap(), 42);
        assert_eq!(calls.get(), 2);
    }

    /// 老库里的 JSON 没有 `translation_lang` 字段，反序列化必须成功（默认 None），
    /// 否则升级后打开历史工程会整条记录读不出来。
    #[test]
    fn legacy_json_without_translation_lang_still_loads() {
        let db = Database::open(":memory:").expect("内存数据库应能打开");
        // 手工写入一份「旧格式」字幕 JSON：只有 translation，没有 translation_lang
        let legacy = r#"[{"index":1,"start":0.0,"end":1.0,"text":"你好","translation":"Hello","polished":"","language":null,"confidence":null,"speaker":null}]"#;
        db.lock_conn()
            .execute(
                "INSERT INTO tasks (file_path, file_name, duration, status, segments_json) VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params!["D:/old.mp4", "old.mp4", 1.0, "completed", legacy],
            )
            .unwrap();

        let loaded = db.load_task_segments(1).unwrap();
        assert_eq!(loaded.len(), 1, "旧格式必须能解析");
        assert_eq!(loaded[0].translation.as_deref(), Some("Hello"));
        assert_eq!(loaded[0].translation_lang, None);
        // 缺语言标记 → 保守判为「不是当前目标语言」，会重译而不是静默沿用
        assert!(!loaded[0].translation_matches("English"));
    }

    // ───────────────────────── 迁移 / 连接健壮性回归 ─────────────────────────

    /// 测试用临时目录（进程内唯一；调用方负责清理）。
    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "v2w_{tag}_{}_{}",
            std::process::id(),
            unique_suffix()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 极简唯一定位符：避免测试之间共用同一个文件名而互相踩。
    fn unique_suffix() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    }

    /// 建一个**最老形状**的库：只有 `tasks` 表（数据列止于 `segments_json`，连
    /// `created_at` 都没有——这是迁移必须覆盖的最坏情况）。真实磁盘库
    /// `voice2word.db` 是「`user_version=0` 但已带 `created_at`/`metrics_json`」的
    /// 中间态，由 `migration_backfills_only_blank_created_at` 覆盖。
    fn create_v0_db(path: &std::path::Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE tasks (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 file_path TEXT NOT NULL,
                 file_name TEXT NOT NULL,
                 duration REAL DEFAULT 0.0,
                 status TEXT NOT NULL,
                 segments_json TEXT NOT NULL
             );
             INSERT INTO tasks (file_path, file_name, duration, status, segments_json)
                 VALUES ('D:/legacy/a.mp4', 'a.mp4', 12.5, 'completed', '[]');
             INSERT INTO tasks (file_path, file_name, duration, status, segments_json)
                 VALUES ('D:/legacy/b.mp4', 'b.mp4', 3.0, 'failed', '[]');",
        )
        .unwrap();
        // 前置断言：这确实是 v0
        let v: i32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 0, "前置条件：v0 库版本号必须为 0");
        for col in ["metrics_json", "content_hash", "created_at"] {
            assert!(
                !Database::column_exists(&conn, col).unwrap(),
                "前置条件：v0 库不应有 {col} 列"
            );
        }
    }

    fn user_version(db: &Database) -> i32 {
        db.lock_conn()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap()
    }

    /// `tasks` 表上是否存在名为 `name` 的**索引**（查 sqlite_master，不是探列）。
    fn index_exists(db: &Database, name: &str) -> bool {
        let conn = db.lock_conn();
        conn.query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'index' AND name = ?1",
            params![name],
            |_| Ok(()),
        )
        .optional()
        .unwrap()
        .is_some()
    }

    /// v0 → v3：最老的库（`user_version=0`、只有 `tasks` 表、缺 `created_at`）迁移后
    /// 必须：版本推进到 3、补齐 `created_at`/`metrics_json`/`content_hash` 三列、建上
    /// `idx_tasks_content_hash`、原有行一条不少，**并且历史列表与缓存查询真的可用**
    /// ——「列齐了但查询仍报 `no such column: created_at`」正是本次修复的缺口。
    #[test]
    fn v0_db_migrates_to_current_schema() {
        let dir = tmp_dir("v0_mig");
        let path = dir.join("legacy_v0.db");
        create_v0_db(&path);

        let db = Database::open(&path).expect("v0 库应能迁移后打开");

        assert_eq!(
            user_version(&db),
            Database::SCHEMA_VERSION,
            "版本号应推进到当前版本"
        );
        let conn = db.lock_conn();
        assert!(
            Database::column_exists(&conn, "metrics_json").unwrap(),
            "metrics_json 应补齐"
        );
        assert!(
            Database::column_exists(&conn, "content_hash").unwrap(),
            "content_hash 应补齐"
        );
        for col in ["segment_count", "sample_text", "has_translation"] {
            assert!(
                Database::column_exists(&conn, col).unwrap(),
                "{col} 应补齐（v3→v4）"
            );
        }
        drop(conn);
        assert!(
            index_exists(&db, "idx_tasks_content_hash"),
            "内容指纹索引应建上"
        );
        assert!(index_exists(&db, "idx_tasks_file_path"), "路径索引也应在");

        // 原有数据仍在，且能用新列查询
        let n: i64 = db
            .lock_conn()
            .query_row("SELECT count(*) FROM tasks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2, "迁移不应丢数据");
        let legacy: i64 = db
            .lock_conn()
            .query_row(
                "SELECT count(*) FROM tasks WHERE content_hash IS NULL AND status = 'completed'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(legacy, 1, "老行的 content_hash 应为 NULL（尚未算指纹）");

        // 关键回归：迁移后必须能把历史列表**真的读出来**。
        //
        // 修复前这里必然失败：老库缺 `created_at`，而 `list_recent_tasks` 的 SELECT
        // 里写死了它 → `no such column: created_at`。原先的断言只看列/索引/版本号，
        // 恰好绕开了这条真实查询路径，于是「测试全绿、功能全坏」。
        let listed = db
            .list_recent_tasks(10)
            .expect("迁移后 list_recent_tasks 必须可用（旧实现会报 no such column: created_at）");
        assert_eq!(listed.len(), 2, "两条老记录都应出现在列表里");
        let names: Vec<&str> = listed.iter().map(|t| t.file_name.as_str()).collect();
        assert!(
            names.contains(&"a.mp4") && names.contains(&"b.mp4"),
            "列表应包含迁移前的行，实际：{names:?}"
        );
        for t in &listed {
            // 老行没有时间戳 → 必须被回填成与 `CURRENT_TIMESTAMP` 同形状的值，
            // 否则 `library.rs` 的 `created_at[..10]` 日期切片会拿到空串。
            assert!(
                t.created_at.len() >= 10,
                "老行 created_at 应被回填成 YYYY-MM-DD HH:MM:SS，实际：{:?}",
                t.created_at
            );
        }
        // 列表查询不装载正文，摘要由 v3→v4 回填的物化列给出（老行是空数组）
        assert!(
            listed
                .iter()
                .all(|t| t.segment_count == 0 && t.sample_text.is_empty() && !t.has_translation),
            "老行的 segments_json 是 []，摘要应为空且未翻译"
        );
        let blanks: i64 = db
            .lock_conn()
            .query_row(
                "SELECT count(*) FROM tasks WHERE created_at IS NULL OR created_at = ''",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(blanks, 0, "回填后不应残留空时间戳");

        // `find_cached_task` 走的是另一条含 `created_at` 的 SELECT，同样必须先于
        // 本次修复就会报错。`D:/legacy/a.mp4` 并不存在 → 指纹为 None → 退回按路径
        // 命中；命中的老行 `segments_json` 是 `[]` → 视为「没有可用字幕」，返回 None。
        // 这里断言的是「查询本身不报错」，而不是命中与否。
        let cached = db
            .find_cached_task("D:/legacy/a.mp4")
            .expect("迁移后 find_cached_task 必须可用（旧实现会报 no such column: created_at）");
        assert!(cached.is_none(), "空字幕的老行不应算作缓存命中");

        // 迁移后新列可读写
        let id = db
            .insert_task(
                "D:/legacy/c.mp4",
                "c.mp4",
                1.0,
                "completed",
                &seg("迁移后"),
                None,
            )
            .unwrap();
        assert_eq!(db.load_task_segments(id).unwrap()[0].text, "迁移后");
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 迁移后的库必须能完整走一遍生命周期——只验证「列 / 索引 / 版本号」是不够的：
    /// 查询侧的 SELECT 列清单与建表语句是两处独立的真相，漏一列只在真实查询时才炸。
    ///
    /// 覆盖：插入 → 列表 → 按指纹命中 → 加载字幕 → 更新 → 删除。
    #[test]
    fn migrated_db_supports_full_lifecycle() {
        let dir = tmp_dir("mig_lifecycle");
        let path = dir.join("legacy_v0.db");
        create_v0_db(&path);

        // 用真实文件：`insert_task` 与 `find_cached_task` 都按内容指纹命中，
        // 文件不存在时指纹为 None，覆盖不到指纹那条 SELECT。
        let media = dir.join("clip.mp4");
        std::fs::write(&media, vec![7u8; 4096]).unwrap();
        let media_path = media.to_string_lossy().to_string();

        let db = Database::open(&path).expect("v0 库应能迁移后打开");

        // 1) 插入
        let mut first = Segment::new(1, 0.0, 1.0, "第一句");
        first.translation = Some("first".to_string());
        first.translation_lang = Some("English".to_string());
        let segs = vec![first, Segment::new(2, 1.0, 2.0, "第二句")];
        let id = db
            .insert_task(&media_path, "clip.mp4", 2.0, "completed", &segs, None)
            .expect("迁移后插入必须可用");

        // 新行必须自带真实时间戳（历史库的列默认值是空串，见 `apply_migrations`）
        let created_at: String = db
            .lock_conn()
            .query_row(
                "SELECT created_at FROM tasks WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            created_at.len() >= 10,
            "新插入的行应有真实时间戳，实际：{created_at:?}"
        );

        // 2) 列表
        let listed = db.list_recent_tasks(10).expect("列表查询必须可用");
        assert_eq!(listed.len(), 3, "两条老记录 + 一条新记录");
        let newest = &listed[0];
        assert_eq!(newest.id, id, "列表按 id 倒序，新记录应在最前");
        assert_eq!(newest.segment_count, 2);
        assert_eq!(
            newest.sample_text, "第一句",
            "摘录取首句原文（polished 为空）"
        );
        assert!(newest.has_translation, "首句带译文，列表记录应报告已翻译");
        assert!(!newest.segments_loaded, "列表查询不装载正文");

        // 3) 按内容指纹命中：另一个路径、同一份内容
        let copy = dir.join("clip_copy.mp4");
        std::fs::write(&copy, vec![7u8; 4096]).unwrap();
        let hit = db
            .find_cached_task(&copy.to_string_lossy())
            .expect("指纹查询必须可用")
            .expect("同内容不同路径应命中缓存");
        assert_eq!(hit.id, id);
        assert!(hit.segments_loaded, "缓存命中会带出完整字幕");

        // 4) 加载字幕
        let loaded = db.load_task_segments(id).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].translation.as_deref(), Some("first"));

        // 5) 更新（按 id 写回编辑结果）
        db.update_task_segments(id, &[Segment::new(1, 0.0, 3.0, "改过的句子")])
            .unwrap();
        assert_eq!(db.load_task_segments(id).unwrap()[0].text, "改过的句子");

        // 6) 删除
        db.delete_task(id).unwrap();
        assert!(db.load_task_segments(id).unwrap().is_empty());
        assert!(db.list_recent_tasks(10).unwrap().iter().all(|t| t.id != id));

        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 回填的**边界**：真实磁盘库 `voice2word.db` 其实**已经有** `created_at` 列
    /// （历史 Python 版建的，值是真的），只有列不存在的老库才需要 `ALTER`。回填
    /// 必须只动「空 / NULL」的行，绝不能把真实时间戳覆盖成迁移那一刻的时间。
    #[test]
    fn migration_backfills_only_blank_created_at() {
        let dir = tmp_dir("mig_backfill");
        let path = dir.join("legacy_v1.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE tasks (
                     id INTEGER PRIMARY KEY AUTOINCREMENT,
                     file_path TEXT NOT NULL,
                     file_name TEXT NOT NULL,
                     duration REAL DEFAULT 0.0,
                     status TEXT NOT NULL,
                     segments_json TEXT NOT NULL,
                     metrics_json TEXT,
                     created_at DATETIME DEFAULT CURRENT_TIMESTAMP
                 );
                 INSERT INTO tasks (file_path, file_name, duration, status, segments_json, created_at)
                     VALUES ('D:/k/real.mp4', 'real.mp4', 1.0, 'completed', '[]', '2026-09-19 02:58:21');
                 INSERT INTO tasks (file_path, file_name, duration, status, segments_json, created_at)
                     VALUES ('D:/k/null.mp4', 'null.mp4', 1.0, 'completed', '[]', NULL);
                 INSERT INTO tasks (file_path, file_name, duration, status, segments_json, created_at)
                     VALUES ('D:/k/blank.mp4', 'blank.mp4', 1.0, 'completed', '[]', '');
                 PRAGMA user_version = 1;",
            )
            .unwrap();
        }

        let db = Database::open(&path).expect("v1 库应能迁移后打开");
        assert_eq!(user_version(&db), Database::SCHEMA_VERSION);

        let rows: Vec<(String, String)> = {
            let conn = db.lock_conn();
            let mut stmt = conn
                .prepare("SELECT file_name, created_at FROM tasks ORDER BY id")
                .unwrap();
            let mapped = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
                .unwrap();
            mapped.map(|r| r.unwrap()).collect()
        };
        assert_eq!(rows[0].1, "2026-09-19 02:58:21", "真实时间戳不得被回填覆盖");
        for (name, ts) in &rows[1..] {
            assert_eq!(
                ts.len(),
                19,
                "{name} 的空时间戳应被回填成 YYYY-MM-DD HH:MM:SS，实际：{ts:?}"
            );
        }

        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 幂等：同一个 v0 库连续 `open` 两次，第二次不能再跑一遍 DDL，
    /// 也不能把版本号推得更高/报错。
    #[test]
    fn reopening_migrated_db_is_idempotent() {
        let dir = tmp_dir("mig_idem");
        let path = dir.join("legacy_v0.db");
        create_v0_db(&path);

        let first = Database::open(&path).expect("第一次打开应成功");
        assert_eq!(user_version(&first), Database::SCHEMA_VERSION);
        drop(first);

        let second = Database::open(&path).expect("第二次打开也应成功（幂等）");
        assert_eq!(
            user_version(&second),
            Database::SCHEMA_VERSION,
            "版本不应被重复推进"
        );
        assert!(index_exists(&second, "idx_tasks_content_hash"));
        let n: i64 = second
            .lock_conn()
            .query_row("SELECT count(*) FROM tasks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2, "重复打开不应改数据");

        // 同一进程内再开一个连接（模拟「两个实例」）也不应互相破坏
        let third = Database::open(&path).expect("并发实例打开应成功");
        assert_eq!(user_version(&third), Database::SCHEMA_VERSION);
        drop(second);
        drop(third);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 多实例**并发**打开同一个老库时，两边都必须成功。
    ///
    /// 复现的是「补列」这一步的 check-then-act 竞态：早期 `init_tables` 在事务外
    /// 先 `column_exists(metrics_json)` 再 `ALTER TABLE ADD COLUMN`。两个实例若在
    /// 同一窗口里都走这步，后到的那次会以 `duplicate column name` 失败——那是逻辑
    /// 错误、不触发 busy 重试，`open` 直接返回 Err，程序连界面都进不去。
    /// 现在补列只发生在 `apply_migrations` 的 `BEGIN IMMEDIATE` 事务里：抢到锁的
    /// 一方先改，另一方等锁、拿锁后重新探列、看到列已在即跳过。
    ///
    /// 用 `Barrier` 让两个线程尽量同时进入 `open`，并把该动作重复几轮来提高触发
    /// 概率；断言两轮都成功、且最终 schema 完整（`metrics_json` 已由迁移补上）。
    #[test]
    fn concurrent_open_of_legacy_db_both_succeed() {
        let dir = tmp_dir("mig_race");
        for round in 0..5 {
            let path = dir.join(format!("legacy_race_{round}.db"));
            create_v0_db(&path);

            let barrier = Arc::new(std::sync::Barrier::new(2));
            let mut handles = Vec::new();
            for _ in 0..2 {
                let path = path.clone();
                let barrier = barrier.clone();
                handles.push(std::thread::spawn(move || {
                    barrier.wait();
                    Database::open(&path).map(|db| user_version(&db))
                }));
            }
            for handle in handles {
                let version = handle
                    .join()
                    .expect("打开线程不应 panic")
                    .expect("并发打开同一老库都应成功");
                assert_eq!(version, Database::SCHEMA_VERSION);
            }

            // 迁移结果完整：老库缺的 metrics_json 已由迁移补齐。
            let db = Database::open(&path).expect("串行重开也应成功");
            assert!(
                Database::column_exists(&db.lock_conn(), "metrics_json").unwrap(),
                "并发迁移后 metrics_json 列必须存在"
            );
            let n: i64 = db
                .lock_conn()
                .query_row("SELECT count(*) FROM tasks", [], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 2, "并发打开不应丢行");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 迁移**原子性**：构造一次「跑到一半会失败」的迁移，断言失败后库自洽。
    ///
    /// 构造手法：预先把 `idx_tasks_content_hash` 这个名字占给一张**表**
    /// （`CREATE INDEX ... IF NOT EXISTS` 只在「已有同名索引」时静默跳过，遇到
    /// 同名表/视图会如实报错），且 `user_version=1` 表示 `content_hash` 列已存在。
    /// 于是 v1→v2 的 `ALTER` 被跳过、`CREATE INDEX` 必然失败。
    /// 断言：错误被返回（不吞）、版本停在 1、`content_hash` 列**没有**被半途加上
    /// ——绝不会出现「版本推进了但索引没建」或「列加了但版本没推」的残局。
    #[test]
    fn failed_migration_rolls_back_atomically() {
        let dir = tmp_dir("mig_atomic");
        let path = dir.join("malformed_v1.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE tasks (
                     id INTEGER PRIMARY KEY AUTOINCREMENT,
                     file_path TEXT NOT NULL,
                     file_name TEXT NOT NULL,
                     duration REAL DEFAULT 0.0,
                     status TEXT NOT NULL,
                     segments_json TEXT NOT NULL,
                     metrics_json TEXT
                 );
                 CREATE TABLE idx_tasks_content_hash (dummy INTEGER);
                 INSERT INTO tasks (file_path, file_name, duration, status, segments_json)
                     VALUES ('D:/x.mp4', 'x.mp4', 1.0, 'completed', '[]');
                 PRAGMA user_version = 1;",
            )
            .unwrap();
        }

        let err = Database::open(&path).expect_err("索引名被表占用时迁移必须如实失败");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("idx_tasks_content_hash"),
            "错误应指向那条失败的 DDL，实际：{msg}"
        );

        // 失败后状态自洽：版本没推进、列没被半途加上
        let conn = Connection::open(&path).unwrap();
        let v: i32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 1, "失败的迁移不得推进 user_version");
        assert!(
            !Database::column_exists(&conn, "content_hash").unwrap(),
            "失败的迁移不得留下半加的列"
        );
        assert!(
            !Database::column_exists(&conn, "created_at").unwrap(),
            "失败的迁移不得留下半加的 created_at 列"
        );
        let n: i64 = conn
            .query_row("SELECT count(*) FROM tasks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "回滚不应丢数据");
        drop(conn);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 中毒容忍：持锁线程 panic 让 Mutex 中毒后，DB 操作仍必须可用，
    /// 而不是此后每一次访问都连环 panic。
    #[test]
    fn db_survives_poisoned_mutex() {
        let dir = tmp_dir("poison");
        let path = dir.join("poison.db");
        let db = Database::open(&path).unwrap();
        db.insert_task("D:/p.mp4", "p.mp4", 1.0, "completed", &seg("中毒前"), None)
            .unwrap();

        // 在另一线程里持锁 panic → Mutex 中毒
        let db_clone = db.clone();
        let joiner = std::thread::spawn(move || {
            let _guard = db_clone.lock_conn();
            panic!("故意在持锁时 panic，制造 Mutex 中毒");
        });
        assert!(joiner.join().is_err(), "子线程应因 panic 而失败");
        assert!(db.conn.is_poisoned(), "前置条件：Mutex 应已中毒");

        // 中毒之后：
        // 1) 读
        assert_eq!(db.load_task_segments(1).unwrap()[0].text, "中毒前");
        // 2) 写
        let id = db
            .insert_task(
                "D:/p2.mp4",
                "p2.mp4",
                1.0,
                "completed",
                &seg("中毒后"),
                None,
            )
            .unwrap();
        assert!(id > 0);
        // 3) 列表查询（读物化的三个摘要列）
        assert_eq!(db.list_recent_tasks(10).unwrap().len(), 2);
        // 4) 收尾维护（原先唯一处理 poison 的方法，现统一走 lock_conn）
        db.maintain_on_shutdown();
        // 5) 删除
        db.delete_task(id).unwrap();

        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `column_exists` 自身的行为：活着的新库三列齐全，v0 形状的库三个都缺。
    #[test]
    fn column_exists_probe_is_accurate() {
        let live = Database::open(":memory:").unwrap();
        {
            let conn = live.lock_conn();
            for col in [
                "metrics_json",
                "content_hash",
                "created_at",
                "segment_count",
                "sample_text",
                "has_translation",
            ] {
                assert!(Database::column_exists(&conn, col).unwrap(), "{col} 应存在");
            }
            assert!(!Database::column_exists(&conn, "definitely_not_a_column").unwrap());
        }
        drop(live);

        let dir = tmp_dir("col_probe");
        let path = dir.join("v0.db");
        create_v0_db(&path);
        let conn = Connection::open(&path).unwrap();
        for col in [
            "metrics_json",
            "content_hash",
            "created_at",
            "segment_count",
            "sample_text",
            "has_translation",
        ] {
            assert!(
                !Database::column_exists(&conn, col).unwrap(),
                "v0 库不应有 {col}"
            );
        }
        drop(conn);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ───────────── 可空列（duration / created_at）容错读回归 ─────────────

    /// 老库/外部工具可能留下 duration = NULL 的行。list_recent_tasks 若按必填
    /// f64 读，`?` 会把整张历史列表变成 Err，调用方 unwrap_or_default() 后
    /// 用户看到的是「视频库整个空了」。
    #[test]
    fn null_duration_row_does_not_break_library_list() {
        let db = Database::open(":memory:").expect("内存数据库应能打开");
        let id = db
            .insert_task("D:/n.mp4", "n.mp4", 1.0, "completed", &seg("有字幕"), None)
            .unwrap();
        db.lock_conn()
            .execute(
                "UPDATE tasks SET duration = NULL WHERE id = ?1",
                params![id],
            )
            .unwrap();

        let listed = db
            .list_recent_tasks(10)
            .expect("duration 为 NULL 的行不应让整个列表查询报错");
        assert_eq!(listed.len(), 1, "坏行自身也要照常列出，而不是让列表为空");
        assert_eq!(listed[0].duration, 0.0, "NULL duration 应容错降级为 0.0");
        assert_eq!(listed[0].file_name, "n.mp4");
    }

    /// 同一类坏行经 find_cached_task（走 row_to_task）也不能把「命中」变成 Err，
    /// 否则调用方 .ok() 会把它吞成「未命中」，用户被迫把已转写过的文件重转一遍。
    #[test]
    fn null_duration_row_does_not_break_cache_lookup() {
        let db = Database::open(":memory:").expect("内存数据库应能打开");
        let id = db
            .insert_task("D:/n.mp4", "n.mp4", 1.0, "completed", &seg("有字幕"), None)
            .unwrap();
        db.lock_conn()
            .execute(
                "UPDATE tasks SET duration = NULL WHERE id = ?1",
                params![id],
            )
            .unwrap();

        // 文件不存在 → 指纹为 None → 退回路径匹配；该行 content_hash 为 NULL，属老记录分支。
        let hit = db
            .find_cached_task("D:/n.mp4")
            .expect("duration 为 NULL 的行不应让缓存查询报错")
            .expect("该行有字幕，应算缓存命中");
        assert_eq!(hit.duration, 0.0, "NULL duration 应容错降级为 0.0");
        assert_eq!(hit.segments[0].text, "有字幕");
    }

    /// created_at 同为可空列：一条 NULL 行不能把列表与缓存两条读路径都拖成 Err。
    #[test]
    fn null_created_at_row_does_not_break_list_or_cache() {
        let db = Database::open(":memory:").expect("内存数据库应能打开");
        let id = db
            .insert_task("D:/c.mp4", "c.mp4", 1.0, "completed", &seg("字幕"), None)
            .unwrap();
        db.lock_conn()
            .execute(
                "UPDATE tasks SET created_at = NULL WHERE id = ?1",
                params![id],
            )
            .unwrap();

        let listed = db
            .list_recent_tasks(10)
            .expect("created_at 为 NULL 的行不应让列表查询报错");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].created_at, "", "NULL created_at 应容错降级为空串");

        let hit = db
            .find_cached_task("D:/c.mp4")
            .expect("created_at 为 NULL 的行不应让缓存查询报错")
            .expect("该行有字幕，应算缓存命中");
        assert_eq!(hit.created_at, "");
    }

    /// 写侧：MAX(duration, ?2) 在 SQLite 里遇 NULL 仍得 NULL，坏行永远修不回。
    /// 编辑保存是唯一的自然修复路径，必须能把 NULL 救成末句 end，同时保持
    /// 「duration 只增不减」的既有口径。
    #[test]
    fn update_task_segments_repairs_null_duration() {
        let db = Database::open(":memory:").expect("内存数据库应能打开");
        let id = db
            .insert_task("D:/u.mp4", "u.mp4", 1.0, "completed", &seg("x"), None)
            .unwrap();
        db.lock_conn()
            .execute(
                "UPDATE tasks SET duration = NULL WHERE id = ?1",
                params![id],
            )
            .unwrap();

        db.update_task_segments(id, &[Segment::new(1, 0.0, 9.0, "y")])
            .unwrap();
        let d: Option<f64> = db
            .lock_conn()
            .query_row(
                "SELECT duration FROM tasks WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(d, Some(9.0), "编辑保存应把 NULL duration 修成末句 end");

        db.update_task_segments(id, &[Segment::new(1, 0.0, 2.0, "z")])
            .unwrap();
        let d: Option<f64> = db
            .lock_conn()
            .query_row(
                "SELECT duration FROM tasks WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            d,
            Some(9.0),
            "末句变短时 duration 不应回退（沿用 MAX 口径）"
        );
    }

    /// 全空白的可选字段既不应被当成「真有译文」，也不应顶掉列表摘要的正文。
    ///
    /// 两条路径各锁一条：
    /// 1. 反序列化归一——`translation_lang = Some("\t")` 会让
    ///    `translation_matches()` 判真，界面出现「已翻译」的空行；全空白
    ///    `polished` 也会让 `display_text()` 的判空逻辑多绕一圈。
    /// 2. 列表摘要 SQL——`polished = "  \n "` 若只认空串的 `NULLIF(..., '')`，
    ///    就会被 `list_recent_tasks` 当成首句正文带进历史列表。SQLite 的
    ///    `TRIM(X)` 默认只去半角空格、不去 `\n` / `\t`，所以 SQL 侧必须用
    ///    显式字符集 trim。
    #[test]
    fn 空白段落字段装载时归一化() {
        let db = Database::open(":memory:").expect("内存数据库应能打开");
        let mut blank = Segment::new(1, 0.0, 1.0, "正文");
        blank.polished = "  \n ".to_string();
        blank.translation = Some("   ".to_string());
        blank.translation_lang = Some("\t".to_string());
        blank.language = Some(" \r\n".to_string());
        let id = db
            .insert_task(
                "D:/blank.mp4",
                "blank.mp4",
                1.0,
                "completed",
                &[blank],
                None,
            )
            .unwrap();

        let loaded = db.load_task_segments(id).unwrap();
        assert_eq!(loaded.len(), 1, "装载回来的句数应与插入一致");
        let seg = &loaded[0];
        assert!(seg.polished.is_empty(), "全空白 polished 应归一为空串");
        assert!(seg.translation.is_none(), "全空白译文应归一为 None");
        assert!(
            seg.translation_lang.is_none(),
            "全空白目标语言应归一为 None"
        );
        assert!(seg.language.is_none(), "全空白识别语言应归一为 None");
        assert_eq!(seg.display_text(), "正文", "归一后应回退到原始正文");

        let recent = db.list_recent_tasks(1).unwrap();
        assert_eq!(
            recent[0].sample_text, "正文",
            "列表摘要应穿透全空白 polished 取正文"
        );
    }
    /// 视频库「仅已翻译 / 仅未翻译」筛选依赖列表记录自带的翻译状态：
    /// `list_recent_tasks` 不装载正文，此前空壳记录的 `has_translation()` 恒为
    /// false，「仅已翻译」永远空、「仅未翻译」恒等于全部。现在 SQL 逐句判定并把
    /// 结果带进记录，筛选必须与装载后的口径一致（纯空白译文不算「有译文」）。
    #[test]
    fn library_translated_filter_uses_loaded_records() {
        use crate::utils::library_query::{query, LibraryFilter, LibrarySort};

        let db = Database::open(":memory:").expect("内存数据库应能打开");

        let mut translated = Segment::new(1, 0.0, 1.0, "你好");
        translated.translation = Some("Hello".to_string());
        let translated_id = db
            .insert_task("D:/v/a.mp4", "a.mp4", 1.0, "completed", &[translated], None)
            .unwrap();

        let plain_id = db
            .insert_task(
                "D:/v/b.mp4",
                "b.mp4",
                1.0,
                "completed",
                &seg("没翻译"),
                None,
            )
            .unwrap();

        let mut blank = Segment::new(1, 0.0, 1.0, "空译文");
        blank.translation = Some("   ".to_string());
        let blank_id = db
            .insert_task("D:/v/c.mp4", "c.mp4", 1.0, "completed", &[blank], None)
            .unwrap();

        let recent = db.list_recent_tasks(10).unwrap();
        assert_eq!(recent.len(), 3);
        // 前置条件：列表记录仍是空壳（不装载正文），翻译状态必须来自 SQL。
        assert!(recent.iter().all(|t| !t.segments_loaded));
        assert!(recent.iter().all(|t| t.segments.is_empty()));
        let flag = |id: i64| {
            recent
                .iter()
                .find(|t| t.id == id)
                .map(|t| t.has_translation)
                .unwrap()
        };
        assert!(flag(translated_id), "有非空译文的记录应为已翻译");
        assert!(!flag(plain_id), "没有任何译文的记录应为未翻译");
        assert!(
            !flag(blank_id),
            "纯空白译文与 Segment::has_translation 同口径，不算已翻译"
        );

        let translated_only = LibraryFilter {
            translated_only: true,
            ..LibraryFilter::default()
        };
        assert_eq!(
            query(&recent, &translated_only, LibrarySort::NewestFirst),
            vec![translated_id],
            "「仅已翻译」应只留下真正带译文的记录"
        );

        let untranslated_only = LibraryFilter {
            untranslated_only: true,
            ..LibraryFilter::default()
        };
        // 列表按 id DESC（最新在前），未翻译的两条即 blank_id → plain_id。
        assert_eq!(
            query(&recent, &untranslated_only, LibrarySort::NewestFirst),
            vec![blank_id, plain_id],
            "「仅未翻译」应留下没译文与纯空白译文的记录"
        );

        // 装载正文后的记录也必须给出相同答案（hydrate 路径重算标志）。
        let hydrated = db.hydrate_task(&recent[0]);
        assert!(hydrated.segments_loaded);
        assert_eq!(
            hydrated.has_translation,
            flag(recent[0].id),
            "装载正文后的翻译状态必须与列表空壳一致"
        );
    }

    // ───────── 列表摘要物化列：写入侧维护 / 迁移回填 / 读侧纯 SELECT ─────────

    /// 旧实现口径：三个派生字段直接用 SQLite 的 JSON 函数从 `segments_json` 现算。
    /// 新实现把它们物化成列；测试拿这个参照物验证「物化值与旧口径等价」。
    fn legacy_summary(db: &Database, id: i64) -> (i64, String, i64) {
        db.lock_conn()
            .query_row(
                "SELECT CASE WHEN json_valid(segments_json)
                             THEN COALESCE(json_array_length(segments_json), 0) ELSE 0 END,
                        CASE WHEN json_valid(segments_json)
                             THEN COALESCE(NULLIF(TRIM(json_extract(segments_json, '$[0].polished'),
                                                      ' ' || char(9) || char(10) || char(11) || char(12) || char(13)), ''),
                                           json_extract(segments_json, '$[0].text'), '') ELSE '' END,
                        CASE WHEN json_valid(segments_json)
                             THEN CASE WHEN EXISTS (
                                      SELECT 1 FROM json_each(segments_json) AS seg
                                      WHERE TRIM(COALESCE(json_extract(seg.value, '$.translation'), ''),
                                                 ' ' || char(9) || char(10) || char(11) || char(12) || char(13)) <> ''
                                  ) THEN 1 ELSE 0 END
                             ELSE 0 END
                 FROM tasks WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap()
    }

    /// 新实现读法：直接读 `insert_task` / `update_task_segments` / 迁移落下的物化列。
    fn stored_summary(db: &Database, id: i64) -> (i64, String, i64) {
        db.lock_conn()
            .query_row(
                "SELECT segment_count, sample_text, has_translation FROM tasks WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap()
    }

    /// v3 → v4 迁移必须把三个派生列回填到与 `segments_json` 一致，且口径与旧的
    /// 「SQL 现算」逐行等价——空数组、有效润色、纯空白译文这些边界都不能漂。
    #[test]
    fn v3_db_backfills_derived_columns() {
        let dir = tmp_dir("v3_to_v4");
        let path = dir.join("legacy_v3.db");

        let mut polished = Segment::new(1, 0.0, 1.0, "原始文本");
        polished.polished = "  润色稿 \n".to_string();
        let mut translated = polished.clone();
        translated.translation = Some("\t译文 ".to_string());
        let mut blank_tl = Segment::new(1, 0.0, 1.0, "空译文");
        blank_tl.translation = Some("   ".to_string());
        let rows: Vec<(&str, String)> = vec![
            (
                "empty.mp4",
                serde_json::to_string(&Vec::<Segment>::new()).unwrap(),
            ),
            (
                "plain.mp4",
                serde_json::to_string(&seg("只有正文")).unwrap(),
            ),
            (
                "polished.mp4",
                serde_json::to_string(&vec![translated]).unwrap(),
            ),
            (
                "blank_tl.mp4",
                serde_json::to_string(&vec![blank_tl]).unwrap(),
            ),
            // 外部工具写坏的 JSON：旧实现按 `json_valid` 降级成 0 / '' / 0，
            // 回填必须给同一答案，而不是让整段迁移失败。
            ("broken.mp4", "not-json".to_string()),
        ];
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE tasks (
                     id INTEGER PRIMARY KEY AUTOINCREMENT,
                     file_path TEXT NOT NULL,
                     file_name TEXT NOT NULL,
                     duration REAL DEFAULT 0.0,
                     status TEXT NOT NULL,
                     segments_json TEXT NOT NULL,
                     metrics_json TEXT,
                     created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                     content_hash TEXT
                 );
                 PRAGMA user_version = 3;",
            )
            .unwrap();
            for (name, json) in &rows {
                conn.execute(
                    "INSERT INTO tasks (file_path, file_name, duration, status, segments_json, created_at)
                     VALUES (?1, ?2, 1.0, 'completed', ?3, '2026-10-01 00:00:00')",
                    params![format!("D:/v3/{name}"), name, json],
                )
                .unwrap();
            }
            for col in ["segment_count", "sample_text", "has_translation"] {
                assert!(
                    !Database::column_exists(&conn, col).unwrap(),
                    "前置条件：v3 库不应有 {col} 列"
                );
            }
        }

        let db = Database::open(&path).expect("v3 库应能迁移后打开");
        assert_eq!(
            user_version(&db),
            Database::SCHEMA_VERSION,
            "版本应推进到 v4"
        );
        {
            let conn = db.lock_conn();
            for col in ["segment_count", "sample_text", "has_translation"] {
                assert!(
                    Database::column_exists(&conn, col).unwrap(),
                    "v3→v4 应补齐 {col} 列"
                );
            }
        }

        // 回填口径 = 旧 SQL 口径：三列与 segments_json 逐行对齐
        for id in 1..=rows.len() as i64 {
            assert_eq!(
                stored_summary(&db, id),
                legacy_summary(&db, id),
                "第 {id} 行的回填值必须与旧实现口径一致"
            );
        }
        assert_eq!(
            stored_summary(&db, 1),
            (0, String::new(), 0),
            "空数组应回填成 0 / 空串 / 0"
        );
        assert_eq!(
            stored_summary(&db, 2),
            (1, "只有正文".to_string(), 0),
            "单句无译文：摘录取正文、未翻译"
        );
        assert_eq!(
            stored_summary(&db, 3),
            (1, "润色稿".to_string(), 1),
            "有效润色稿应 trim 后作为摘录，译文应算已翻译"
        );
        assert_eq!(
            stored_summary(&db, 4),
            (1, "空译文".to_string(), 0),
            "纯空白译文不算已翻译"
        );
        assert_eq!(
            stored_summary(&db, 5),
            (0, String::new(), 0),
            "坏 JSON 应按旧口径降级为 0 / 空串 / 0"
        );

        // 列表查询直接读列：返回的三个字段必须与逐行回填值一致
        let listed = db.list_recent_tasks(10).unwrap();
        assert_eq!(listed.len(), rows.len());
        for task in &listed {
            assert_eq!(
                (
                    task.segment_count as i64,
                    task.sample_text.clone(),
                    i64::from(task.has_translation),
                ),
                stored_summary(&db, task.id),
                "列表记录 {} 的派生字段应直接来自物化列",
                task.file_name
            );
        }

        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 写入侧（`insert_task` / `update_task_segments`）必须让三个派生列与
    /// `segments_json` 始终一致：空字幕、全空白译文、有效润色都要当场算对，
    /// 更新后同步刷新——读侧已经不再解析 JSON 兜底。
    #[test]
    fn derived_columns_stay_in_sync_with_segments_json() {
        let db = Database::open(":memory:").expect("内存数据库应能打开");

        // 1) 空 segments：三列取零值
        let empty = db
            .insert_task("D:/d/empty.mp4", "empty.mp4", 0.0, "completed", &[], None)
            .unwrap();
        assert_eq!(stored_summary(&db, empty), (0, String::new(), 0));
        assert_eq!(stored_summary(&db, empty), legacy_summary(&db, empty));

        // 2) 首句带有效润色（含首尾空白）、次句纯空白译文：
        //    摘录取 trim 后的润色稿，纯空白译文不算已翻译
        let mut first = Segment::new(1, 0.0, 1.0, "原文");
        first.polished = "  润色后  ".to_string();
        let mut second = Segment::new(2, 1.0, 2.0, "第二句");
        second.translation = Some("   ".to_string());
        let id = db
            .insert_task(
                "D:/d/two.mp4",
                "two.mp4",
                2.0,
                "completed",
                &[first, second],
                None,
            )
            .unwrap();
        assert_eq!(stored_summary(&db, id), (2, "润色后".to_string(), 0));
        assert_eq!(stored_summary(&db, id), legacy_summary(&db, id));

        // 3) 更新为「一句 + 有效译文」：三列跟着刷新
        let mut updated = Segment::new(1, 0.0, 3.0, "改过");
        updated.translation = Some("Hello".to_string());
        db.update_task_segments(id, &[updated]).unwrap();
        assert_eq!(stored_summary(&db, id), (1, "改过".to_string(), 1));
        assert_eq!(stored_summary(&db, id), legacy_summary(&db, id));

        // 4) 再清空：不能残留上一轮的句数与译文标志
        db.update_task_segments(id, &[]).unwrap();
        assert_eq!(stored_summary(&db, id), (0, String::new(), 0));
        assert_eq!(stored_summary(&db, id), legacy_summary(&db, id));
    }

    /// `list_recent_tasks` 的语义必须与旧的「SQL 现算」完全等价：它现在只 SELECT
    /// 物化列，因此对同一批边界数据，返回的三个派生字段应与旧口径逐一相等。
    #[test]
    fn list_recent_tasks_matches_legacy_json_derivation() {
        let db = Database::open(":memory:").expect("内存数据库应能打开");

        let cases: Vec<Vec<Segment>> = vec![
            vec![],
            seg("没有译文"),
            {
                let mut s = Segment::new(1, 0.0, 1.0, "带译文");
                s.translation = Some("Hello".to_string());
                s.translation_lang = Some("English".to_string());
                vec![s]
            },
            {
                let mut s = Segment::new(1, 0.0, 1.0, "带润色但无译文");
                s.polished = "  Polished  ".to_string();
                vec![s]
            },
            {
                let mut s = Segment::new(1, 0.0, 1.0, "多句首句");
                s.polished = "  ".to_string();
                let mut t = Segment::new(2, 1.0, 2.0, "多句次句");
                t.translation = Some("\n".to_string());
                vec![s, t]
            },
        ];
        let ids: Vec<i64> = cases
            .iter()
            .enumerate()
            .map(|(i, segments)| {
                db.insert_task(
                    &format!("D:/l/{i}.mp4"),
                    &format!("{i}.mp4"),
                    1.0,
                    "completed",
                    segments,
                    None,
                )
                .unwrap()
            })
            .collect();

        let listed = db.list_recent_tasks(10).unwrap();
        assert_eq!(listed.len(), ids.len());
        assert!(listed
            .iter()
            .all(|t| !t.segments_loaded && t.segments.is_empty()));
        for task in &listed {
            assert_eq!(
                (
                    task.segment_count as i64,
                    task.sample_text.clone(),
                    i64::from(task.has_translation),
                ),
                legacy_summary(&db, task.id),
                "记录 {} 的三个派生字段必须与旧实现口径等价",
                task.file_name
            );
        }

        // 抽查两个边界：有效润色稿应被 trim 后作为摘录；首句 polished 全是空白
        // 时摘录回退到首句正文，次句的 "\n" 不算译文。
        assert_eq!(stored_summary(&db, ids[3]), (1, "Polished".to_string(), 0));
        assert_eq!(stored_summary(&db, ids[4]), (2, "多句首句".to_string(), 0));
    }

    // ───────── 坏 segments_json：不再静默变成「字幕空了」 ─────────

    /// 库里 `segments_json` 被外部改坏（历史版本、手动改库、磁盘损坏）时，装载必须
    /// 明确失败，而不是 `.ok().unwrap_or_default()` 成空表让用户以为「这份没字幕」。
    /// 批量导出（`ui/actions.rs`）已按 `Err` 分支提示失败原因，所以返回 `Err` 就等于
    /// 把原因带到用户眼前。
    #[test]
    fn bad_segments_json_load_returns_err_instead_of_empty_table() {
        let db = Database::open(":memory:").expect("内存数据库应能打开");
        let id = db
            .insert_task(
                "D:/bad.mp4",
                "bad.mp4",
                1.0,
                "completed",
                &seg("原本有字幕"),
                None,
            )
            .unwrap();
        // 模拟外部写坏：JSON 数组被截断
        db.lock_conn()
            .execute(
                "UPDATE tasks SET segments_json = ?1 WHERE id = ?2",
                params!["[{\"index\":1,\"start\":0.0,", id],
            )
            .unwrap();

        let err = db
            .load_task_segments(id)
            .expect_err("坏 JSON 必须返回 Err，而不是静默变空表");
        let text = format!("{err:#}");
        assert!(
            text.contains(&id.to_string()),
            "错误必须带上 task id 以便定位，实际: {text}"
        );

        // 报告式接口：不返回 Err，但原因必须带得出来
        let report = db.load_task_segments_report(id).unwrap();
        assert!(report.segments.is_empty());
        assert!(report.failed(), "报告式接口必须标出装载失败");
        let decode_error = report.decode_error.expect("必须带出解码失败原因");
        assert_eq!(decode_error.task_id, id);
        assert!(decode_error.raw_len > 0, "原始字节数要能报出来");
        assert!(
            decode_error.to_string().contains(&id.to_string()),
            "Display 必须可读且能定位到记录: {decode_error}"
        );
    }

    /// 行不存在 / 列为 NULL / 空串都是「这份工程确实没有字幕」：不能升级成错误，
    /// 否则历史库里合法的空壳记录会被判成坏数据。
    #[test]
    fn missing_or_empty_segments_json_is_not_an_error() {
        let db = Database::open(":memory:").expect("内存数据库应能打开");
        assert!(
            db.load_task_segments(9999).unwrap().is_empty(),
            "不存在的 id 应为空表"
        );
        let id = db
            .insert_task("D:/empty.mp4", "empty.mp4", 1.0, "completed", &[], None)
            .unwrap();
        db.lock_conn()
            .execute(
                "UPDATE tasks SET segments_json = '' WHERE id = ?1",
                params![id],
            )
            .unwrap();
        let report = db.load_task_segments_report(id).unwrap();
        assert!(report.segments.is_empty());
        assert!(report.decode_error.is_none(), "空串不是坏数据");
        assert!(db.load_task_segments(id).unwrap().is_empty());
    }

    /// 纯函数口径：坏 JSON 与「没有字幕」必须分开，且坏数据的原始长度要如实报出。
    /// 正常 JSON 的解析结果一个字都不许变（本次改动只把失败分离出来）。
    #[test]
    fn decode_segments_json_separates_corruption_from_absence() {
        assert!(decode_segments_json(7, None).1.is_none());
        assert!(decode_segments_json(7, Some("")).1.is_none());
        assert!(decode_segments_json(7, Some("  \n ")).1.is_none());

        let (segments, err) = decode_segments_json(7, Some("{{不是 JSON"));
        assert!(segments.is_empty());
        let err = err.expect("坏数据必须给出原因");
        assert_eq!(err.task_id, 7);
        assert_eq!(err.raw_len, "{{不是 JSON".len());

        let raw = r#"[{"index":1,"start":0.0,"end":1.0,"text":"句子","language":null}]"#;
        let (ok, none) = decode_segments_json(7, Some(raw));
        assert!(none.is_none(), "合法 JSON 不是失败");
        assert_eq!(ok.len(), 1);
        assert_eq!(ok[0].text, "句子");
    }

    /// `hydrate_task` 的签名不能返回 `Result`（调用方 `AppState::load_task_with`
    /// 直接吃 `TaskRecord`），所以它在坏数据上必须**保留列表摘要**并明确标回
    /// 「未装载」，而不是伪装成「装载成功、只是没有字幕」；需要把原因显示给用户的
    /// 调用点改走 `try_hydrate_task`。
    #[test]
    fn hydrate_task_keeps_list_summary_and_try_variant_reports_error() {
        let db = Database::open(":memory:").expect("内存数据库应能打开");
        let id = db
            .insert_task("D:/h.mp4", "h.mp4", 1.0, "completed", &seg("摘要句"), None)
            .unwrap();
        let shell = db
            .list_recent_tasks(10)
            .unwrap()
            .into_iter()
            .find(|t| t.id == id)
            .expect("列表里应有刚插入的记录");
        assert_eq!(shell.segment_count, 1);
        assert_eq!(shell.sample_text, "摘要句");

        db.lock_conn()
            .execute(
                "UPDATE tasks SET segments_json = 'not json' WHERE id = ?1",
                params![id],
            )
            .unwrap();

        assert!(
            db.try_hydrate_task(&shell).is_err(),
            "try_hydrate_task 必须把坏数据报成 Err"
        );

        let degraded = db.hydrate_task(&shell);
        assert_eq!(degraded.segment_count, 1, "列表里的句数摘要必须保留");
        assert_eq!(degraded.sample_text, "摘要句", "列表里的摘录必须保留");
        assert!(degraded.segments.is_empty());
        assert!(
            !degraded.segments_loaded,
            "装载失败不能伪装成「已装载且没有字幕」"
        );
    }

    /// 正常路径不许被上面的改动带偏：合法 JSON 经 `load_task_segments`、
    /// `load_task_segments_report`、`try_hydrate_task` 三条路径必须给出同一份字幕
    /// （含空白归一）。
    #[test]
    fn valid_segments_json_loads_identically_on_every_path() {
        let db = Database::open(":memory:").expect("内存数据库应能打开");
        let mut first = Segment::new(1, 0.0, 1.0, "第一句");
        first.translation = Some("   ".to_string()); // 纯空白译文要在装载时被归一
        let id = db
            .insert_task(
                "D:/ok.mp4",
                "ok.mp4",
                2.0,
                "completed",
                &[first, Segment::new(2, 1.0, 2.0, "第二句")],
                None,
            )
            .unwrap();

        let direct = db.load_task_segments(id).unwrap();
        let report = db.load_task_segments_report(id).unwrap();
        assert!(!report.failed());
        assert_eq!(direct, report.segments, "两条装载路径必须给同一份字幕");
        assert_eq!(direct.len(), 2);
        assert!(direct[0].translation.is_none(), "空白译文应被归一为 None");

        let shell = db
            .list_recent_tasks(10)
            .unwrap()
            .into_iter()
            .find(|t| t.id == id)
            .unwrap();
        assert!(!shell.segments_loaded);
        let hydrated = db.try_hydrate_task(&shell).unwrap();
        assert!(hydrated.segments_loaded);
        assert_eq!(hydrated.segments, direct, "hydrate 与直接装载必须一致");
        assert_eq!(hydrated.segment_count, 2);
        assert_eq!(hydrated.sample_text, "第一句");
    }

    /// 后台写库路径：直接写调用方给的 JSON（不二次序列化），派生列与 Vec 路径一致；
    /// 坏 JSON 必须**拒绝入库**——它是「保存失败」这一可见状态的唯一来源，
    /// 绝不能把损坏数据盖到已有字幕上。
    #[test]
    fn update_task_segments_json_writes_payload_and_derived_columns() {
        let db = Database::open(":memory:").expect("内存数据库应能打开");
        let id = db
            .insert_task("D:/j.mp4", "j.mp4", 1.0, "completed", &seg("旧句"), None)
            .unwrap();

        let mut first = Segment::new(1, 0.0, 2.0, "新句");
        first.translation = Some("Hello".to_string());
        let payload = serde_json::to_string(&vec![first]).unwrap();
        db.update_task_segments_json(id, &payload).unwrap();

        let loaded = db.load_task_segments(id).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].text, "新句");
        assert_eq!(loaded[0].translation.as_deref(), Some("Hello"));
        let stored: String = db
            .lock_conn()
            .query_row(
                "SELECT segments_json FROM tasks WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stored, payload, "写回的字节必须与调用方给的一致");
        assert_eq!(stored_summary(&db, id), (1, "新句".to_string(), 1));

        let err = db
            .update_task_segments_json(id, "{ 不是 JSON")
            .expect_err("坏 JSON 必须返回 Err");
        assert!(
            format!("{err:#}").contains(&id.to_string()),
            "报错要能定位到记录"
        );
        assert_eq!(
            db.load_task_segments(id).unwrap()[0].text,
            "新句",
            "写失败时不得改动库里已有的字幕"
        );
    }
}

/// 派生列一致性回归：**迁移回填**与**写入路径**对同一份 `segments_json` 必须给出同一个
/// `segment_count` / `sample_text` / `has_translation`。
///
/// 钉住的是「只由非 ASCII 空白（NBSP / 全角空格 / 行分隔符…）组成的译文」这个入口：写入路径
/// 走 `Segment::has_translation`（`str::trim`，Unicode White_Space）算「没译文」，而迁移回填
/// 若用 ASCII 空白集 `TRIM` 就会把它算成「已翻译」——同一份字幕，列表页徽标与实际状态分叉。
///
/// 测试模块刻意不用 `use super::*`：只显式取用需要的两个内部符号，避免把整层私有项拉进作用域。
#[cfg(test)]
mod derived_column_consistency {
    use super::{summarize_segments, Database};
    use crate::subtitle::Segment;
    use rusqlite::{params, Connection};

    fn read_summary(db: &Database, id: i64) -> (i64, String, i64) {
        db.lock_conn()
            .query_row(
                "SELECT segment_count, sample_text, has_translation FROM tasks WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap()
    }

    /// 在 v3 库（缺三个派生列）里塞一行，再让 `Database::open` 触发迁移回填。
    fn migrated_summary(tag: &str, segments_json: &str) -> (i64, String, i64) {
        let dir = std::env::temp_dir().join(format!("v2w_uni_ws_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("v3.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE tasks (
                     id INTEGER PRIMARY KEY AUTOINCREMENT,
                     file_path TEXT NOT NULL,
                     file_name TEXT NOT NULL,
                     duration REAL DEFAULT 0.0,
                     status TEXT NOT NULL,
                     segments_json TEXT NOT NULL,
                     metrics_json TEXT,
                     created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                     content_hash TEXT
                 );
                 PRAGMA user_version = 3;",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO tasks (file_path, file_name, duration, status, segments_json, created_at)
                 VALUES (?1, ?2, 1.0, 'completed', ?3, '2026-10-01 00:00:00')",
                params![format!("D:/u/{tag}.mp4"), "u.mp4", segments_json],
            )
            .unwrap();
        }
        let db = Database::open(&path).expect("v3 库应能迁移后打开");
        let summary = read_summary(&db, 1);
        drop(db);
        // 收尾：连带 -wal / -shm 一起删，不给下一次测试留垃圾。
        let _ = std::fs::remove_dir_all(&dir);
        summary
    }

    /// 纯空白译文（含非 ASCII 空白）在两条路径下都必须算「未翻译」，且逐字相等。
    #[test]
    fn blank_translations_agree_between_migration_and_write_path() {
        // 前三个是 ASCII 空白（旧口径也判空）；后三个是 NBSP / 全角空格 / 行分隔符——
        // 正是旧 ASCII 口径会误判成「已翻译」的那类。
        let cases = ["   ", "\t", "\n", "\u{00A0}", "\u{3000}", "\u{2028}"];
        for (i, translation) in cases.iter().enumerate() {
            let mut seg = Segment::new(1, 0.0, 1.0, "正文");
            seg.translation = Some((*translation).to_string());
            let segments = vec![seg];

            // 写入路径：`insert_task` 物化出来的派生列。
            let mem = Database::open(":memory:").expect("内存库应能打开");
            let id = mem
                .insert_task(
                    &format!("D:/u/{i}.mp4"),
                    "u.mp4",
                    1.0,
                    "completed",
                    &segments,
                    None,
                )
                .expect("插入应成功");
            let written = read_summary(&mem, id);

            // 迁移回填路径：同一份 JSON 装进 v3 库再升级。
            let json = serde_json::to_string(&segments).unwrap();
            let backfilled = migrated_summary(&format!("case{i}"), &json);

            assert_eq!(
                backfilled, written,
                "译文 {translation:?} 的派生列在迁移回填与写入路径下必须逐字相等"
            );
            assert_eq!(written.2, 0, "纯空白译文 {translation:?} 不该算已翻译");
            // 交叉验证：写入路径的答案就是 `summarize_segments` 的答案（唯一口径）。
            assert_eq!(i64::from(summarize_segments(&segments).2), written.2);
        }
    }
}
