//! 一次性探针：打印指定 exe 的「缺失导入 DLL」列表（启动体检同款判定）。
//! 用法：cargo run --offline --example pe_imports_probe -- <path-to-exe>
fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("用法: pe_imports_probe <exe>");
    let p = std::path::Path::new(&path);
    println!("entry = {}", p.display());
    println!("exists = {}", p.exists());
    let missing = voice2word::utils::pe_imports::missing_imports(p);
    if missing.is_empty() {
        println!("missing = (none) -> 运行库齐备，启动体检不会报警");
    } else {
        println!("missing = {missing:?}");
    }
    let bytes = std::fs::read(p).expect("read");
    let names = voice2word::utils::pe_imports::imported_dll_names(&bytes);
    println!("direct imports = {names:?}");
}
