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
}
