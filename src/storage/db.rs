//! SQLite 数据库存储模块

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::sync::{Arc, Mutex};

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
    pub metrics: Option<crate::core::PipelinePerformanceMetrics>,
}

#[derive(Clone)]
pub struct Database {
    conn: Arc<Mutex<Connection>>,
}

impl Database {
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

        let db = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        db.init_tables()?;
        Ok(db)
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

        conn.execute(
            "INSERT INTO tasks (file_path, file_name, duration, status, segments_json, metrics_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![file_path, file_name, duration, status, segments_json, metrics_json],
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
            "SELECT id, file_path, file_name, duration, status, created_at, metrics_json,
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
                segment_count: row.get::<_, i64>(7).unwrap_or(0).max(0) as usize,
                sample_text: row.get(8).unwrap_or_default(),
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

    /// 查找指定文件的历史已完成转写记录 (用于 0 秒智能缓存命中)
    pub fn find_cached_task(&self, file_path: &str) -> Result<Option<TaskRecord>> {
        let conn = self.conn.lock().unwrap();
        let normalized_1 = file_path.replace('\\', "/");
        let normalized_2 = file_path.replace('/', "\\");
        let mut stmt = conn.prepare(
            "SELECT id, file_path, file_name, duration, status, segments_json, created_at, metrics_json 
             FROM tasks 
             WHERE (file_path = ?1 OR file_path = ?2) AND status = 'completed' 
             ORDER BY id DESC LIMIT 1",
        )?;
        let mut rows = stmt.query_map(params![normalized_1, normalized_2], |row| {
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
                metrics,
            })
        })?;

        if let Some(r) = rows.next() {
            let record = r?;
            if !record.segments.is_empty() {
                return Ok(Some(record));
            }
        }
        Ok(None)
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