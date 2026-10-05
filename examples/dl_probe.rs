//! 手动验证下载器。
//!   cargo run --example dl_probe -- list            列出全部条目与就位状态
//!   cargo run --example dl_probe -- <item-id>       下载单个条目
//!   cargo run --example dl_probe -- all-missing     按体积从小到大补齐全部缺失
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "list".to_string());
    let items = voice2word::utils::ITEMS;

    if mode == "list" {
        // 走 PresenceContext（配置只读一次），与 AppState::refresh_model_presence 同路径。
        // 逐条调 is_present 会让每个条目各读一次 config.toml。
        let ctx = voice2word::utils::model_download::PresenceContext::load();
        println!("{:<22} {:>10} {:>8}  {}", "id", "size", "present", "dest");
        for i in items {
            println!(
                "{:<22} {:>10} {:>8}  {}",
                i.id,
                i.size,
                if ctx.is_present(i) { "yes" } else { "NO" },
                i.dest
            );
        }
        println!(
            "\n缺失总数={} 其中必需={}",
            voice2word::utils::model_download::missing_count(),
            voice2word::utils::model_download::missing_required_count()
        );
        return;
    }

    let cancel = Arc::new(AtomicBool::new(false));
    let mut targets: Vec<&voice2word::utils::DownloadItem> = if mode == "all-missing" {
        let mut v: Vec<_> = items
            .iter()
            .filter(|i| !voice2word::utils::model_download::is_present(i))
            .collect();
        v.sort_by_key(|i| i.size);
        v
    } else {
        vec![items.iter().find(|i| i.id == mode).unwrap_or_else(|| panic!("未知条目 {mode}"))]
    };

    for item in targets.drain(..) {
        println!("── {} ({}) ──", item.label, item.dest);
        let last = std::sync::Mutex::new(0u64);
        let cb: voice2word::utils::model_download::ProgressFn = Box::new(move |done, total, _| {
            let mut l = last.lock().unwrap();
            if done - *l >= 16 * 1024 * 1024 || done == total {
                *l = done;
                println!("   {:.1} / {:.1} MB", done as f64 / 1048576.0, total as f64 / 1048576.0);
            }
        });
        let t0 = std::time::Instant::now();
        match voice2word::utils::model_download::download_one(item, &cancel, Some(&cb)) {
            Ok(p) => println!(
                "   OK {:.1}s  {}",
                t0.elapsed().as_secs_f64(),
                p.display()
            ),
            Err(e) => println!("   FAIL {e:#}"),
        }
    }
    println!(
        "\n完成后：缺失总数={} 其中必需={}",
        voice2word::utils::model_download::missing_count(),
        voice2word::utils::model_download::missing_required_count()
    );
}