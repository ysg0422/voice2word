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
    /// `segment_count` / `sample_text` 两个由 SQLite 直接算出的摘要字段；真正需要
    /// 字幕内容时（载入工程、导出）再按 `id` 调 `load_task_segments` 取。
    pub segments: Vec<Segment>,
    /// 该工程的字幕总句数（列表查询由 `json_array_length` 直接算出）
    pub segment_count: usize,
    /// 首句字幕摘录，供视频库卡片预览
    pub sample_text: String,
    /// `segments` 是否已装载。为 `false` 时表示来自列表查询，内容为空壳。
    pub segments_loaded: bool,
    pub created_at: String,
    /// 媒体内容的采样指纹（见 `utils::fingerprint`）。老记录或无法读取的文件为 `None`。
    /// 缓存的**主键**是它，而不是 `file_path`——同内容改名/复制能命中，同路径换内容
    /// 会正确地失效。
    pub content_hash: Option<String>,
    pub metrics: Option<crate::core::PipelinePerformanceMetrics>,
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
    pub const SCHEMA_VERSION: i32 = 2;

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
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP
            );
            "#,
            [],
        )?;
        // 兼容老数据库自动增加 metrics_json 列
        if !Database::column_exists(&conn, "metrics_json")? {
            conn.execute("ALTER TABLE tasks ADD COLUMN metrics_json TEXT;", [])?;
        }

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
    /// 一旦被吞掉，程序就会带着一个**版本号撒谎的库**继续跑（以为是 v2，其实
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
        // 为什么不新增 v3：`SCHEMA_VERSION=2` 是上一个提交刚引入的，v2 从未发布，
        // 真实世界里不存在「user_version=2 且缺 created_at」的库（真实老库全是
        // v0/v1）。新增 v3 就得再写一段专门照顾这种并不存在的库；把补列并入 v1→v2，
        // 则 v0/v1 两种老库一步到位，版本号仍停在 2。
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
                "INSERT INTO tasks (file_path, file_name, duration, status, segments_json, metrics_json, content_hash, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![file_path, file_name, duration, status, segments_json, metrics_json, content_hash, created_at],
            )
        })?;

        Ok(conn.last_insert_rowid())
    }

    /// 列出最近任务（**不装载字幕正文**）。
    ///
    /// 句数与首句摘录交给 SQLite 的 JSON 函数直接算，避免把每条工程的
    /// `segments_json` 全量反序列化——历史库越大，这一项越是启动瓶颈。
    pub fn list_recent_tasks(&self, limit: usize) -> Result<Vec<TaskRecord>> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(
            "SELECT id, file_path, file_name, duration, status, created_at, metrics_json, content_hash,
                    CASE WHEN json_valid(segments_json)
                         THEN COALESCE(json_array_length(segments_json), 0) ELSE 0 END,
                    CASE WHEN json_valid(segments_json)
                         THEN COALESCE(NULLIF(json_extract(segments_json, '$[0].polished'), ''),
                                       json_extract(segments_json, '$[0].text'), '') ELSE '' END
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
                duration: row.get(3)?,
                status: row.get(4)?,
                created_at: row.get(5)?,
                metrics,
                segments: Vec::new(),
                segment_count: row.get::<_, i64>(8).unwrap_or(0).max(0) as usize,
                sample_text: row.get(9).unwrap_or_default(),
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

    /// 按 id 装载某条工程的完整字幕（仅在真正需要正文时调用）
    pub fn load_task_segments(&self, id: i64) -> Result<Vec<Segment>> {
        let conn = self.lock_conn();
        let json: Option<String> = conn
            .query_row(
                "SELECT segments_json FROM tasks WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(json
            .as_deref()
            .and_then(|s| serde_json::from_str::<Vec<Segment>>(s).ok())
            .unwrap_or_default())
    }

    /// 把一条列表记录补齐为「已装载字幕」的完整记录（失败时保留原记录）
    pub fn hydrate_task(&self, task: &TaskRecord) -> TaskRecord {
        if task.segments_loaded {
            return task.clone();
        }
        let segments = self.load_task_segments(task.id).unwrap_or_default();
        let mut out = task.clone();
        out.segment_count = segments.len();
        out.segments = segments;
        out.segments_loaded = true;
        out
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
        with_busy_retry(|| {
            conn.execute(
                "UPDATE tasks SET segments_json = ?1, duration = MAX(duration, ?2) WHERE id = ?3",
                params![segments_json, total_dur, id],
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
        let segments: Vec<Segment> = serde_json::from_str(&segments_json).unwrap_or_default();
        let metrics_json: Option<String> = row.get(7).unwrap_or(None);
        let metrics: Option<crate::core::PipelinePerformanceMetrics> = metrics_json
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok());
        let sample_text = segments
            .first()
            .map(|s| s.display_text().to_string())
            .unwrap_or_default();
        Ok(TaskRecord {
            id: row.get(0)?,
            file_path: row.get(1)?,
            file_name: row.get(2)?,
            duration: row.get(3)?,
            status: row.get(4)?,
            segment_count: segments.len(),
            sample_text,
            segments_loaded: true,
            segments,
            created_at: row.get(6)?,
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

    /// v0 → v2：最老的库（`user_version=0`、只有 `tasks` 表、缺 `created_at`）迁移后
    /// 必须：版本推进到 2、补齐 `created_at`/`metrics_json`/`content_hash` 三列、建上
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
            "版本号应推进到 v2"
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
        // 列表查询不装载正文，摘要由 SQLite 的 JSON 函数算出（老行是空数组）
        assert!(
            listed
                .iter()
                .all(|t| t.segment_count == 0 && t.sample_text.is_empty()),
            "老行的 segments_json 是 []，摘要应为空"
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
        // 3) 列表查询（走 segments_json 的 JSON 函数路径）
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
            for col in ["metrics_json", "content_hash", "created_at"] {
                assert!(Database::column_exists(&conn, col).unwrap(), "{col} 应存在");
            }
            assert!(!Database::column_exists(&conn, "definitely_not_a_column").unwrap());
        }
        drop(live);

        let dir = tmp_dir("col_probe");
        let path = dir.join("v0.db");
        create_v0_db(&path);
        let conn = Connection::open(&path).unwrap();
        for col in ["metrics_json", "content_hash", "created_at"] {
            assert!(
                !Database::column_exists(&conn, col).unwrap(),
                "v0 库不应有 {col}"
            );
        }
        drop(conn);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
