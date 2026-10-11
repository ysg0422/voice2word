use std::fs;
use std::path::Path;

fn main() {
    // 确保 target/release/voice2word.exe 在 clippy / check 时即便尚未构建也有占位文件，
    // 消除 setup.rs 的 include_bytes! 循环依赖导致的冷编译中断。
    let target_dir = Path::new("target/release");
    let exe_path = target_dir.join("voice2word.exe");
    if !exe_path.exists() {
        let _ = fs::create_dir_all(target_dir);
        let _ = fs::write(&exe_path, b"");
    }
}
