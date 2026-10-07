//! SQLite 数据库存储模块

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::sync::{Arc, Mutex};

use tracing::{info, warn};

use crate::subtitle::Segment;

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

#[derive(Clone)]
pub struct Database {
    conn: Arc<Mutex<Connection>>,
}

impl Database {
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
        let conn = self.conn.lock().unwrap();
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
        let _ = conn.execute("ALTER TABLE tasks ADD COLUMN metrics_json TEXT;", []);

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
        let _ = conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_tasks_file_path ON tasks(file_path, status);",
            [],
        );
        // 内容指纹索引：缓存命中走 `content_hash = ?`，与路径索引并列。
        // 老库此时可能还没有 content_hash 列，`CREATE INDEX` 会失败——那正是
        // 迁移（apply_migrations）要补的列，故这里忽略错误，迁移后再建一次。
        let _ = conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_tasks_content_hash ON tasks(content_hash, status);",
            [],
        );
        Ok(())
    }

    /// 把库从当前 `user_version` 逐级迁移到 [`Self::SCHEMA_VERSION`]。
    ///
    /// 每段迁移都必须**幂等**（即便版本号因故错位、重复执行也不出错/不重复写入），
    /// 且在一次事务内完成——中途失败则版本号不推进，下次启动重试，不会留下
    /// 「迁移到一半」的半状态。
    fn apply_migrations(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let from: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if from >= Self::SCHEMA_VERSION {
            return Ok(());
        }
        // v0 → v1：历史上靠 `ALTER TABLE ADD COLUMN` 硬加出来的 metrics_json 列，
        // 现在正式纳入版本管理。老库若还没有该列，这里补齐（幂等：忽略 duplicate）。
        if from < 1 {
            let _ = conn.execute("ALTER TABLE tasks ADD COLUMN metrics_json TEXT", []);
        }
        // v1 → v2：加入内容指纹列与索引。
        if from < 2 {
            let _ = conn.execute("ALTER TABLE tasks ADD COLUMN content_hash TEXT", []);
            let _ = conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_tasks_content_hash ON tasks(content_hash, status);",
                [],
            );
        }
        conn.pragma_update(None, "user_version", Self::SCHEMA_VERSION)
            .with_context(|| format!("写入 user_version={} 失败", Self::SCHEMA_VERSION))?;
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
        let conn = self.conn.lock().unwrap();
        let segments_json = serde_json::to_string(segments)
            .context("序列化 segments 失败")?;
        let metrics_json = metrics.and_then(|m| serde_json::to_string(m).ok());
        // 落库时顺手算一次内容指纹（采样哈希，毫秒级）。失败（文件已删/不可读）
        // 就存 NULL，缓存回退到「只按路径」的老行为。
        let content_hash = crate::utils::fingerprint::media_fingerprint(file_path);

        conn.execute(
            "INSERT INTO tasks (file_path, file_name, duration, status, segments_json, metrics_json, content_hash) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![file_path, file_name, duration, status, segments_json, metrics_json, content_hash],
        )?;

        Ok(conn.last_insert_rowid())
    }

    /// 列出最近任务（**不装载字幕正文**）。
    ///
    /// 句数与首句摘录交给 SQLite 的 JSON 函数直接算，避免把每条工程的
    /// `segments_json` 全量反序列化——历史库越大，这一项越是启动瓶颈。
    pub fn list_recent_tasks(&self, limit: usize) -> Result<Vec<TaskRecord>> {
        let conn = self.conn.lock().unwrap();
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
        let conn = self.conn.lock().unwrap();
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
        let conn = self.conn.lock().unwrap();
        let segments_json = serde_json::to_string(segments).context("序列化 segments 失败")?;
        let total_dur = segments.last().map(|s| s.end).unwrap_or(0.0);
        conn.execute(
            "UPDATE tasks SET segments_json = ?1, duration = MAX(duration, ?2) WHERE id = ?3",
            params![segments_json, total_dur, id],
        )?;
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
        let conn = self.conn.lock().unwrap();
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
        let conn = match self.conn.lock() {
            Ok(c) => c,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Err(err) = conn.execute_batch("PRAGMA optimize;") {
            warn!(error = %err, "退出维护 PRAGMA optimize 失败（忽略）");
        }
        match conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))
        }) {
            Ok((busy, log_pages, checkpointed)) => {
                if busy != 0 {
                    info!(log_pages, checkpointed, "退出 checkpoint：库仍被占用，已尽量合并（下次启动继续）");
                } else {
                    info!(log_pages, checkpointed, "退出 checkpoint：WAL 已并回主库并截断");
                }
            }
            Err(err) => warn!(error = %err, "退出 checkpoint 失败（忽略，下次启动会继续）"),
        }
    }

    /// 删除指定历史任务记录
    pub fn delete_task(&self, id: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM tasks WHERE id = ?1", params![id])?;
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
        db.update_task_segments(first, &seg("改过的第一句")).unwrap();

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
            loaded.iter().filter(|s| s.translation_matches("English")).count(),
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
        db.insert_task(&a.to_string_lossy(), "original.mp4", 1.0, "completed", &seg("内容一致"), None)
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
        db.insert_task(&a.to_string_lossy(), "v.mp4", 1.0, "completed", &seg("旧字幕"), None)
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
            .conn
            .lock()
            .unwrap()
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
            let v: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
            assert_eq!(v, 0, "前置：旧库版本应为 0");
        }

        let db = Database::open(&path).expect("老库应能被迁移后打开");
        let v: i32 = db
            .conn
            .lock()
            .unwrap()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, Database::SCHEMA_VERSION, "迁移后版本号应推进");
        // 老数据还在，且 metrics_json 列已补齐（可正常 SELECT）
        let n: i64 = db
            .conn
            .lock()
            .unwrap()
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
            .conn
            .lock()
            .unwrap()
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

    /// 老库里的 JSON 没有 `translation_lang` 字段，反序列化必须成功（默认 None），
    /// 否则升级后打开历史工程会整条记录读不出来。
    #[test]
    fn legacy_json_without_translation_lang_still_loads() {
        let db = Database::open(":memory:").expect("内存数据库应能打开");
        // 手工写入一份「旧格式」字幕 JSON：只有 translation，没有 translation_lang
        let legacy = r#"[{"index":1,"start":0.0,"end":1.0,"text":"你好","translation":"Hello","polished":"","language":null,"confidence":null,"speaker":null}]"#;
        db.conn
            .lock()
            .unwrap()
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
}