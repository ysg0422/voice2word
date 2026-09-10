//! SQLite 数据库存储模块

use anyhow::{Context, Result};
use rusqlite::{params, Connection};
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
    pub segments: Vec<Segment>,
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

    pub fn list_recent_tasks(&self, limit: usize) -> Result<Vec<TaskRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, file_path, file_name, duration, status, segments_json, created_at, metrics_json FROM tasks ORDER BY id DESC LIMIT ?1",
        )?;

        let rows = stmt.query_map([limit], |row| {
            let segments_json: String = row.get(5)?;
            let segments: Vec<Segment> = serde_json::from_str(&segments_json).unwrap_or_default();
            let metrics_json: Option<String> = row.get(7).unwrap_or(None);
            let metrics: Option<crate::core::PipelinePerformanceMetrics> = metrics_json
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok());
            Ok(TaskRecord {
                id: row.get(0)?,
                file_path: row.get(1)?,
                file_name: row.get(2)?,
                duration: row.get(3)?,
                status: row.get(4)?,
                segments,
                created_at: row.get(6)?,
                metrics,
            })
        })?;

        let mut tasks = Vec::new();
        for r in rows {
            tasks.push(r?);
        }
        Ok(tasks)
    }

    /// 更新指定任务的字幕片段列表 (用于在编辑器中修改错字或时间后写回)
    pub fn update_task_segments(&self, file_path: &str, segments: &[Segment]) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let segments_json = serde_json::to_string(segments).context("序列化 segments 失败")?;
        let total_dur = segments.last().map(|s| s.end).unwrap_or(0.0);
        conn.execute(
            "UPDATE tasks SET segments_json = ?1, duration = MAX(duration, ?2) WHERE file_path = ?3",
            params![segments_json, total_dur, file_path],
        )?;
        Ok(())
    }

    /// 删除指定历史任务记录
    pub fn delete_task(&self, id: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM tasks WHERE id = ?1", params![id])?;
        Ok(())
    }
}
