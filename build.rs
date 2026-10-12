use std::fs;
use std::path::Path;

fn main() {
    // 确保 target/release/voice2word.exe 在 clippy / check 时即便尚未构建也有占位文件，
    // 消除 setup.rs 的 include_bytes! 循环依赖导致的冷编译中断。
    //
    // 这里写出的是一个「最小合法 PE 头」占位文件，而不是 0 字节空文件：
    // 0 字节文件在按大小判断产物是否有效的流程里会被误当成真实 exe；
    // 带合法 PE 头的占位文件既能让 include_bytes! 正常编译，又能被工具识别为「仅占位、不可运行」。
    // 仅在文件缺失时写入，cargo build --release 产出的真实 voice2word.exe 不会被覆盖。
    let target_dir = Path::new("target/release");
    let exe_path = target_dir.join("voice2word.exe");
    if !exe_path.exists() {
        let _ = fs::create_dir_all(target_dir);
        let _ = fs::write(&exe_path, minimal_pe_placeholder());
        println!(
            "cargo:warning=voice2word.exe 尚未构建，已写入最小 PE 占位文件（不可运行）：{}",
            exe_path.display()
        );
    }
}

/// 构造仅含合法 PE 头、不含任何节的最小占位文件。
/// 布局：DOS 头(0x40) + "PE\0\0" + COFF 头(20 字节) + PE32+ 可选头(0xF0 字节，含 16 个空数据目录项)。
fn minimal_pe_placeholder() -> Vec<u8> {
    let mut out: Vec<u8> = vec![0u8; 0x40];
    out[0] = b'M';
    out[1] = b'Z';
    // e_lfanew：PE 头相对文件起始的偏移
    out[0x3C..0x40].copy_from_slice(&0x40u32.to_le_bytes());
    out.extend_from_slice(b"PE\0\0");

    // COFF 文件头
    out.extend_from_slice(&0x8664u16.to_le_bytes()); // Machine: AMD64
    out.extend_from_slice(&0u16.to_le_bytes()); // NumberOfSections
    out.extend_from_slice(&0u32.to_le_bytes()); // TimeDateStamp
    out.extend_from_slice(&0u32.to_le_bytes()); // PointerToSymbolTable
    out.extend_from_slice(&0u32.to_le_bytes()); // NumberOfSymbols
    out.extend_from_slice(&0xF0u16.to_le_bytes()); // SizeOfOptionalHeader (PE32+)
    out.extend_from_slice(&0x0002u16.to_le_bytes()); // Characteristics: EXECUTABLE_IMAGE

    // PE32+ 可选头固定部分（0x70 字节）
    let opt_start = out.len();
    out.extend_from_slice(&0x020Bu16.to_le_bytes()); // Magic: PE32+
    out.push(0); // MajorLinkerVersion
    out.push(0); // MinorLinkerVersion
    out.extend_from_slice(&0u32.to_le_bytes()); // SizeOfCode
    out.extend_from_slice(&0u32.to_le_bytes()); // SizeOfInitializedData
    out.extend_from_slice(&0u32.to_le_bytes()); // SizeOfUninitializedData
    out.extend_from_slice(&0u32.to_le_bytes()); // AddressOfEntryPoint
    out.extend_from_slice(&0u32.to_le_bytes()); // BaseOfCode
    out.extend_from_slice(&0x0000_0001_4000_0000u64.to_le_bytes()); // ImageBase
    out.extend_from_slice(&0x1000u32.to_le_bytes()); // SectionAlignment
    out.extend_from_slice(&0x200u32.to_le_bytes()); // FileAlignment
    out.extend_from_slice(&0u16.to_le_bytes()); // MajorOperatingSystemVersion
    out.extend_from_slice(&0u16.to_le_bytes()); // MinorOperatingSystemVersion
    out.extend_from_slice(&0u16.to_le_bytes()); // MajorImageVersion
    out.extend_from_slice(&0u16.to_le_bytes()); // MinorImageVersion
    out.extend_from_slice(&0u16.to_le_bytes()); // MajorSubsystemVersion
    out.extend_from_slice(&0u16.to_le_bytes()); // MinorSubsystemVersion
    out.extend_from_slice(&0u32.to_le_bytes()); // Win32VersionValue
    out.extend_from_slice(&0x2000u32.to_le_bytes()); // SizeOfImage
    out.extend_from_slice(&0x200u32.to_le_bytes()); // SizeOfHeaders
    out.extend_from_slice(&0u32.to_le_bytes()); // CheckSum
    out.extend_from_slice(&0x0002u16.to_le_bytes()); // Subsystem: WINDOWS_GUI
    out.extend_from_slice(&0u16.to_le_bytes()); // DllCharacteristics
    out.extend_from_slice(&0x0010_0000u64.to_le_bytes()); // SizeOfStackReserve
    out.extend_from_slice(&0x0000_1000u64.to_le_bytes()); // SizeOfStackCommit
    out.extend_from_slice(&0x0010_0000u64.to_le_bytes()); // SizeOfHeapReserve
    out.extend_from_slice(&0x0000_1000u64.to_le_bytes()); // SizeOfHeapCommit
    out.extend_from_slice(&0u32.to_le_bytes()); // LoaderFlags
    out.extend_from_slice(&16u32.to_le_bytes()); // NumberOfRvaAndSizes

    // 16 个数据目录项（每个 8 字节）全部为 0，凑满 SizeOfOptionalHeader = 0xF0
    out.resize(opt_start + 0xF0, 0);
    out
}
