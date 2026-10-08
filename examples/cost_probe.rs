//! 量化 `find_cached_task` 的单次成本。
//!
//! 它此前在**渲染路径**上每帧被调用（转写页「已命中缓存 N 句」卡片），
//! 而每次都会把整份 `segments_json` 反序列化出来。这个探针用来复现那个成本，
//! 也是把它挪进 `AppState::cached_transcription` 缓存的实测依据。
//!
//! 用法：
//!   cargo run --release --example cost_probe -- [数据库路径] [媒体文件路径]
//!
//! 两个参数都可省略：数据库默认取当前目录下的 `voice2word.db`，
//! 媒体路径默认取库里最新一条已完成记录（所以本机无需手填路径即可复现）。
use voice2word::storage::Database;

fn main() {
    let mut args = std::env::args().skip(1);
    let db_path = args.next().unwrap_or_else(|| "voice2word.db".to_string());

    let db = Database::open(&db_path).unwrap_or_else(|e| {
        eprintln!("打开数据库失败：{db_path}（{e}）");
        eprintln!("提示：可显式传入数据库路径，例如");
        eprintln!("  cargo run --release --example cost_probe -- <db> [media]");
        std::process::exit(2);
    });

    // 媒体路径：命令行优先；否则取库里最新一条已完成记录，避免硬编码机器路径。
    let path = match args.next() {
        Some(p) => p,
        None => match latest_completed_path(&db) {
            Some(p) => {
                println!("未指定媒体路径，改用库中最新已完成记录：{p}");
                p
            }
            None => {
                eprintln!("库中没有已完成记录，请显式传入一个媒体文件路径。");
                std::process::exit(2);
            }
        },
    };

    // 预热（第一次查询含 SQLite 语句编译与页缓存冷启动，不计入均值）
    let _ = db.find_cached_task(&path);
    let hit = db.find_cached_task(&path).ok().flatten();
    match &hit {
        Some(t) => println!(
            "命中记录：{} 句（字幕 JSON 将整份反序列化）",
            t.segments.len()
        ),
        None => println!("警告：该路径在库中无已完成记录，测得的是「查空」成本，会偏乐观。"),
    }

    let n = 200;
    let t0 = std::time::Instant::now();
    for _ in 0..n {
        let _ = db.find_cached_task(&path);
    }
    let per = t0.elapsed().as_secs_f64() / n as f64;
    println!("find_cached_task: {:.3} ms/次", per * 1000.0);
    println!(
        "若每帧调用（60fps）：每帧 {:.3} ms，占总帧预算(16.7ms) 的 {:.1}%",
        per * 1000.0,
        per * 1000.0 / 16.7 * 100.0
    );
    println!(
        "若每帧调用（80ms 批处理节流 → 12.5fps）：占用 {:>5.1}%",
        per * 1000.0 / 80.0 * 100.0
    );
}

/// 取库里最新一条 `completed` 记录的 `file_path`。
fn latest_completed_path(db: &Database) -> Option<String> {
    db.list_recent_tasks(1)
        .ok()?
        .into_iter()
        .next()
        .map(|t| t.file_path)
}
