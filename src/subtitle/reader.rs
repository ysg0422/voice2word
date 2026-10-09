//! 字幕导入：把 SRT / WebVTT / 纯文本读回 `Vec<Segment>`
//!
//! Voice2Word 能写出八种字幕格式，却一种也读不回来。用户手里有同事发来的 SRT，
//! 或者想修一版自己上次导出的字幕时，只能把整段视频重新转写一遍——又慢又费钱。
//! 本模块补上这条缺失的导入链路。
//!
//! 设计边界：这里**只做纯字符串 → `Vec<Segment>` 的转换**，不碰 UI、不决定读哪个
//! 文件、不做任何 IO 决策，因此可以被单元测试完整覆盖；真正读盘的动作交给调用方，
//! [`read_subtitle_file`] 只是「按扩展名 + 编码兜底」的便捷封装。
//!
//! 容错原则：手工编辑过的字幕几乎必然带一两个坏块（少了时间轴、时间戳写错、
//! 结束时间早于开始时间）。为其中一个坏块让整次导入失败毫无用处——用户真正想要的
//! 是「能救多少救多少」，所以坏块一律跳过；只有整篇连一个可用块都没有时才报错。
//!
//! 为什么 `index` 一定要重排：输入文件里的序号经常在手工增删后错乱甚至重复，
//! 而下游的定位、质检跳转、说话人归属全部按 `index` 检索，信错序号会让整条链路
//! 集体错位。因此输出序号一律按出现顺序重排为 1..n，输入序号只当分块标记用。

use std::path::Path;

use anyhow::{bail, Context, Result};

use super::Segment;

/// 纯文本导入时每个字符占用的秒数（时间戳是**编造**的占位值）。
///
/// TXT 没有任何时间信息，导入后若直接导出 SRT 就会得到一串凭空捏造的时间轴，
/// 因此这里用固定速率给每行估一个时长，并把它明确写成常量：调用方读到
/// [`parse_txt`] 的结果后应当提示用户「时间轴为估算值，需要人工校对」。
const TXT_SECONDS_PER_CHAR: f64 = 0.3;
/// 纯文本导入时每行的最短时长（秒），避免「嗯」「好」这类极短句被压成 0 秒。
const TXT_MIN_DURATION: f64 = 1.0;

/// 支持导入的字幕格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubtitleFormat {
    Srt,
    Vtt,
    Txt,
}

impl SubtitleFormat {
    /// 按扩展名判断（不区分大小写）；不认识的扩展名返回 `None`。
    ///
    /// 返回 `None` 而不是猜一个默认格式：猜错会让用户拿到「导入成功但内容全乱」
    /// 的结果，比直接告诉他这个扩展名不支持要糟得多。
    pub fn from_path(path: &Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        match ext.as_str() {
            "srt" => Some(Self::Srt),
            "vtt" => Some(Self::Vtt),
            "txt" => Some(Self::Txt),
            _ => None,
        }
    }

    /// 界面用的中文名，例如 "SRT 字幕"。
    pub fn label(self) -> &'static str {
        match self {
            Self::Srt => "SRT 字幕",
            Self::Vtt => "WebVTT 字幕",
            Self::Txt => "纯文本",
        }
    }
}

/// 解析 SRT 文本。
///
/// 坏块（无时间轴 / 时间戳不可解析 / `end < start`）一律跳过；只有整篇一个可用
/// 块都没有时才报错。理由见模块头：为手改留下的一个坏块放弃整份文件是没有意义的。
pub fn parse_srt(content: &str) -> Result<Vec<Segment>> {
    let text = normalize_newlines(strip_bom(content));
    let mut segments = Vec::new();
    for block in split_blocks(&text) {
        if let Some(seg) = parse_srt_block(&block) {
            segments.push(seg);
        }
    }
    finalize_segments(segments, "SRT")
}

/// 解析 WebVTT 文本。
///
/// 跳过 `WEBVTT` 头、`NOTE` / `STYLE` / `REGION` 元数据块，忽略 cue 的可选标识行
/// 与行尾 cue 设置（如 `line:0 position:50%`）。
pub fn parse_vtt(content: &str) -> Result<Vec<Segment>> {
    let text = normalize_newlines(strip_bom(content));
    // `WEBVTT` 头单独占一行（后面可能跟标题文字）。先摘掉它，避免「头与第一个 cue
    // 之间没空行」这种写法把整个第一块连头部一起丢掉。
    let body = match text.split_once('\n') {
        Some((first, rest)) if first.trim_start().starts_with("WEBVTT") => rest,
        _ => text.as_str(),
    };
    let mut segments = Vec::new();
    for block in split_blocks(body) {
        let first = block[0].trim_start();
        // NOTE / STYLE / REGION 是 VTT 的元数据块，不是字幕；整块跳过。
        if first.starts_with("NOTE") || first.starts_with("STYLE") || first.starts_with("REGION") {
            continue;
        }
        // cue 可以有可选的标识行，真正的分界仍是含 `-->` 的时间轴行。
        let Some(timing_at) = block.iter().position(|l| l.contains("-->")) else {
            continue;
        };
        let Some((start, end)) = parse_timing_line(block[timing_at]) else {
            continue;
        };
        if end < start {
            continue;
        }
        let text = join_text_lines(&block[timing_at + 1..]);
        segments.push(Segment::new(0, start, end, text));
    }
    finalize_segments(segments, "WebVTT")
}

/// 解析纯文本（每行一句，无时间信息）。
///
/// **时间轴是编造的**：TXT 里没有时间信息，这里按 [`TXT_SECONDS_PER_CHAR`] 给每行
/// 估一个时长（下限 [`TXT_MIN_DURATION`]），并让后一段的起点接在前一段的终点上。
/// 调用方必须把这一点告知用户——把 TXT 导入再导出成 SRT，得到的是假时间轴。
pub fn parse_txt(content: &str) -> Result<Vec<Segment>> {
    let text = normalize_newlines(strip_bom(content));
    let mut segments = Vec::new();
    let mut cursor = 0.0_f64;
    for line in text.split('\n') {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // 按时**字符**数而不是字节数估算：一个汉字占 3 字节，按字节算会把中文行
        // 估成三倍长，时间轴整体漂移。
        let duration = (line.chars().count() as f64 * TXT_SECONDS_PER_CHAR).max(TXT_MIN_DURATION);
        let start = cursor;
        let end = start + duration;
        segments.push(Segment::new(0, start, end, line));
        cursor = end;
    }
    finalize_segments(segments, "纯文本")
}

/// 按格式分派。
pub fn parse(content: &str, format: SubtitleFormat) -> Result<Vec<Segment>> {
    match format {
        SubtitleFormat::Srt => parse_srt(content),
        SubtitleFormat::Vtt => parse_vtt(content),
        SubtitleFormat::Txt => parse_txt(content),
    }
}

/// 读文件并按扩展名解析。
///
/// **编码兜底**：先按 UTF-8 解码，失败则按 GBK 解码（中文字幕常见来源：老播放器
/// 导出、记事本另存、论坛下载的 SRT 都可能是 GBK），两条都失败才报错。
pub fn read_subtitle_file(path: &Path) -> Result<Vec<Segment>> {
    let format = SubtitleFormat::from_path(path).with_context(|| {
        format!(
            "不支持的字幕扩展名（只支持 .srt / .vtt / .txt）：{}",
            path.display()
        )
    })?;
    let bytes =
        std::fs::read(path).with_context(|| format!("读取字幕文件失败: {}", path.display()))?;
    let text = decode_subtitle_bytes(&bytes)?;
    parse(&text, format)
}

/// 编码兜底：先当 UTF-8 解，失败再当 GBK 解，两条都失败才报错。
fn decode_subtitle_bytes(bytes: &[u8]) -> Result<String> {
    if let Ok(text) = std::str::from_utf8(bytes) {
        return Ok(text.to_string());
    }
    let (text, _, had_errors) = encoding_rs::GBK.decode(bytes);
    if had_errors {
        bail!("字幕文件既不是 UTF-8 也不是 GBK 编码，无法解码");
    }
    Ok(text.into_owned())
}

/// 去掉 UTF-8 BOM。
///
/// Windows 记事本另存的字幕常带 BOM，不处理的话第一行的序号或 `WEBVTT` 头会被
/// 当成「带 BOM 的怪文本」，识别失败。
fn strip_bom(content: &str) -> &str {
    content.strip_prefix('\u{FEFF}').unwrap_or(content)
}

/// 统一换行：`\r\n` 与单独的 `\r` 都归一到 `\n`。
///
/// 不做这一步，`\r` 会挂在时间戳行尾（`00:00:01,000 --> 00:00:02,000\r`），
/// 解析时间戳时把 `\r` 也算进去，整块被判为坏块。
fn normalize_newlines(content: &str) -> String {
    content.replace("\r\n", "\n").replace('\r', "\n")
}

/// 把正文切成「空行分隔」的块。
fn split_blocks(content: &str) -> Vec<Vec<&str>> {
    let mut blocks = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    for line in content.split('\n') {
        if line.trim().is_empty() {
            if !current.is_empty() {
                blocks.push(std::mem::take(&mut current));
            }
        } else {
            current.push(line);
        }
    }
    if !current.is_empty() {
        blocks.push(current);
    }
    blocks
}

/// 解析一个 SRT 块。
///
/// 时间轴行之前的内容（通常是数字序号）一律丢弃：手工编辑后的序号经常错乱甚至
/// 重复，信它会让所有按 `index` 检索的下游逻辑整体错位。
fn parse_srt_block(block: &[&str]) -> Option<Segment> {
    let timing_at = block.iter().position(|l| l.contains("-->"))?;
    let (start, end) = parse_timing_line(block[timing_at])?;
    // `end < start` 的倒挂块必须丢：留着会让播放器/剪辑软件算出负时长。
    if end < start {
        return None;
    }
    let text = join_text_lines(&block[timing_at + 1..]);
    Some(Segment::new(0, start, end, text))
}

/// 解析一行时间轴（`A --> B`），并忽略 `B` 后面可能跟的 VTT cue 设置
/// （如 `line:0 position:50%`）。返回 `(start, end)` 秒数。
fn parse_timing_line(line: &str) -> Option<(f64, f64)> {
    let (lhs, rhs) = line.split_once("-->")?;
    let start = parse_timestamp(lhs.trim())?;
    // 只取箭头右边的第一个空白分隔 token，其后的 cue 设置整段忽略。
    let end_token = rhs.split_whitespace().next()?;
    let end = parse_timestamp(end_token)?;
    Some((start, end))
}

/// 把时间戳文本解析成秒。
///
/// 接受 `H:MM:SS,mmm`、`H:MM:SS.mmm`、`MM:SS.mmm`、`MM:SS,mmm` 与无小时的
/// `SS.mmm`；毫秒分隔符 `,`（SRT 传统）与 `.`（VTT 规范）都收。任何解析不出来的
/// 输入返回 `None`——绝不 panic、绝不 unwrap，因为输入是用户手工改过的文件。
fn parse_timestamp(raw: &str) -> Option<f64> {
    let text = raw.trim();
    if text.is_empty() {
        return None;
    }
    // 毫秒部分用最后一个 `.` 或 `,` 切出来，兼容 `00:00:01.500` 与 `00:00:01,500`。
    let (clock, fraction) = match text.rsplit_once(['.', ',']) {
        Some((c, f)) => (c, Some(f)),
        None => (text, None),
    };
    let millis = match fraction {
        Some(f) => {
            // 只认 1~3 位数字：`01:00:00,` 或 `01:00:00,abc` 都判为坏时间戳。
            if f.is_empty() || f.len() > 3 || !f.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let value: f64 = f.parse().ok()?;
            // 一位小数位表示十分之一秒、三位表示毫秒，按位数缩放。
            value / 10f64.powi(f.len() as i32)
        }
        None => 0.0,
    };
    let parts: Vec<&str> = clock.split(':').collect();
    let (hours, minutes, seconds) = match parts.as_slice() {
        [s] => (0u32, 0u32, parse_uint(s)?),
        [m, s] => {
            let s = parse_uint(s)?;
            if s >= 60 {
                return None;
            }
            (0, parse_uint(m)?, s)
        }
        [h, m, s] => {
            let m = parse_uint(m)?;
            let s = parse_uint(s)?;
            if m >= 60 || s >= 60 {
                return None;
            }
            (parse_uint(h)?, m, s)
        }
        _ => return None,
    };
    Some(hours as f64 * 3600.0 + minutes as f64 * 60.0 + seconds as f64 + millis)
}

/// 解析一个非负整数字段（只认纯数字），失败返回 `None`。
fn parse_uint(field: &str) -> Option<u32> {
    if field.is_empty() || !field.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    field.parse().ok()
}

/// 把块里的文本行按 `\n` 拼接，每行只去尾部空白。
///
/// 只去尾部：导出器折行后常留下行尾空格，留着会让文本比对莫名其妙地不等；
/// 行首空格则可能是刻意排版缩进，不该动。
fn join_text_lines(lines: &[&str]) -> String {
    lines
        .iter()
        .map(|l| l.trim_end())
        .collect::<Vec<_>>()
        .join("\n")
}

/// 收尾：空结果报错，否则把序号重排为 1..n。
fn finalize_segments(mut segments: Vec<Segment>, what: &str) -> Result<Vec<Segment>> {
    if segments.is_empty() {
        bail!(
            "未能从 {what} 内容中解析出任何字幕：文件可能是空的，\
             或所有字幕块的时间轴都缺失/损坏"
        );
    }
    for (i, seg) in segments.iter_mut().enumerate() {
        seg.index = i + 1;
    }
    Ok(segments)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// 建一个以进程号 + 标签区分的临时目录，避免并行测试互相踩。
    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("v2w_reader_{}_{}", std::process::id(), tag));
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    /// 最基础的一条：三块标准 SRT 必须解析出三条，时间与文本都要对得上。
    /// 这是导入功能的「hello world」，任何字段错位都会在这里暴露。
    #[test]
    fn srt_basic_three_cues() {
        let content = "\
1
00:00:01,000 --> 00:00:02,000
你好

2
00:00:03,000 --> 00:00:04,500
世界

3
00:00:05,000 --> 00:00:06,000
再见
";
        let segs = parse_srt(content).unwrap();
        assert_eq!(segs.len(), 3);
        assert_eq!(segs[0].index, 1);
        assert_eq!(segs[0].start, 1.0);
        assert_eq!(segs[0].end, 2.0);
        assert_eq!(segs[0].text, "你好");
        assert_eq!(segs[1].index, 2);
        assert_eq!(segs[1].end, 4.5);
        assert_eq!(segs[2].text, "再见");
    }

    /// 序号行必须被忽略并重排为 1..n：手工编辑后序号错乱/重复是常态，
    /// 信任输入序号会让所有按 index 检索的下游逻辑错位。
    #[test]
    fn srt_renumbers_indices_in_output_order() {
        let content = "\
5
00:00:01,000 --> 00:00:02,000
甲

5
00:00:02,000 --> 00:00:03,000
乙

0
00:00:03,000 --> 00:00:04,000
丙
";
        let segs = parse_srt(content).unwrap();
        let indices: Vec<usize> = segs.iter().map(|s| s.index).collect();
        assert_eq!(indices, vec![1, 2, 3]);
        assert_eq!(segs[0].text, "甲");
        assert_eq!(segs[2].text, "丙");
    }

    /// `,` 与 `.` 都当毫秒分隔符收：SRT 传统用逗号，但 VTT 规范用点，
    /// 用户从不同工具导出时会混用，只认一种就会把另一批文件判成坏块。
    #[test]
    fn srt_accepts_comma_and_dot_millisecond_separator() {
        let comma = "1\n00:00:01,250 --> 00:00:02,500\n甲\n";
        let dot = "1\n00:00:01.250 --> 00:00:02.500\n甲\n";
        let a = parse_srt(comma).unwrap();
        let b = parse_srt(dot).unwrap();
        assert_eq!(a[0].start, 1.25);
        assert_eq!(a[0].end, 2.5);
        assert_eq!(b[0].start, a[0].start);
        assert_eq!(b[0].end, a[0].end);
    }

    /// 短格式 `MM:SS.mmm`（无小时）也要收：很多工具在时长不足一小时时省掉小时段。
    #[test]
    fn srt_accepts_short_mm_ss_timestamps() {
        let content = "1\n00:01.500 --> 00:02.000\n短格式\n";
        let segs = parse_srt(content).unwrap();
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].start, 1.5);
        assert_eq!(segs[0].end, 2.0);
    }

    /// 坏块只跳过、不致命：好块必须全部存活。三种坏法（无时间轴 / 时间戳不可
    /// 解析 / 时间倒挂）混在一起测，确保跳过逻辑不会连累相邻的好块。
    #[test]
    fn srt_skips_malformed_blocks_but_keeps_good_ones() {
        let content = "\
1
00:00:01,000 --> 00:00:02,000
好块一

2
这里本该是时间轴

3
99:99:99,999 --> 00:00:04,000
坏时间戳

4
00:00:04,000 --> 00:00:03,000
时间倒挂

5
00:00:05,000 --> 00:00:06,000
好块二
";
        let segs = parse_srt(content).unwrap();
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].text, "好块一");
        assert_eq!(segs[1].text, "好块二");
        assert_eq!(segs[0].index, 1);
        assert_eq!(segs[1].index, 2);
    }

    /// 单独验证 `end < start` 的倒挂块被丢弃：这是手改时间轴最常见的错误，
    /// 留着会让播放器/剪辑软件算出负时长，后续所有时间线操作一起崩。
    #[test]
    fn srt_drops_block_with_end_before_start() {
        let content = "\
1
00:00:05,000 --> 00:00:04,000
倒挂

2
00:00:06,000 --> 00:00:07,000
正常
";
        let segs = parse_srt(content).unwrap();
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].text, "正常");
        assert_eq!(segs[0].index, 1);
    }

    /// 空文件与纯垃圾输入要报错而不是返回空表：返回空表会让调用方以为
    /// 「导入成功但字幕是空的」，用户拿不到任何反馈，比直接报错更糟。
    #[test]
    fn srt_empty_or_garbage_input_errors() {
        assert!(parse_srt("").is_err());
        assert!(parse_srt("\n\n\n").is_err());
        assert!(parse_srt("这不是字幕\n随便写的两行").is_err());
    }

    /// 多行文本用 `\n` 拼接，且只去每行尾部空白：尾部空格是导出器折行时常见的
    /// 残留，留着会让文本比对莫名其妙地不等；行首空格可能是刻意缩进，保留。
    #[test]
    fn srt_joins_multiline_text_and_trims_trailing_space() {
        let content = "1\n00:00:01,000 --> 00:00:02,000\n第一行   \n  第二行\n";
        let segs = parse_srt(content).unwrap();
        assert_eq!(segs[0].text, "第一行\n  第二行");
    }

    /// VTT：头部、NOTE、STYLE 要跳过；cue 的可选标识行与行尾 cue 设置要忽略；
    /// 剩下的文本与时间必须精确。这几种元素经常同时出现，得一起测。
    #[test]
    fn vtt_skips_metadata_and_reads_cues() {
        let content = "\
WEBVTT

NOTE 这是一段注释
注释可以有很多行

STYLE
::cue { color: white }

cue-1
00:00:01.000 --> 00:00:02.500 line:0 position:50%
第一行
第二行

00:00:03.000 --> 00:00:04.000
第三句
";
        let segs = parse_vtt(content).unwrap();
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].index, 1);
        assert_eq!(segs[0].start, 1.0);
        assert_eq!(segs[0].end, 2.5);
        assert_eq!(segs[0].text, "第一行\n第二行");
        assert_eq!(segs[1].start, 3.0);
        assert_eq!(segs[1].end, 4.0);
        assert_eq!(segs[1].text, "第三句");
    }

    /// TXT 每行一句，时间轴是**编造**的占位值：必须单调不减、后一段的起点
    /// 等于前一段的终点，且短行有下限。调用方据此提示用户「时间需要校对」。
    #[test]
    fn txt_synthesizes_monotonic_times() {
        let content = "第一句\n\n第二句比较长一点点\n短\n";
        let segs = parse_txt(content).unwrap();
        assert_eq!(segs.len(), 3);
        assert_eq!(segs[0].index, 1);
        assert_eq!(segs[0].start, 0.0);
        // 空行被跳过，不产生空段
        assert_eq!(segs[0].text, "第一句");
        assert_eq!(segs[1].text, "第二句比较长一点点");
        assert_eq!(segs[2].text, "短");
        assert!(segs[0].end > segs[0].start);
        assert_eq!(segs[1].start, segs[0].end);
        assert_eq!(segs[2].start, segs[1].end);
        // 短句走 1.0 秒下限（浮点累加后只剩 ~1e-16 误差，故用近似比较）
        assert!((segs[2].end - segs[2].start - 1.0).abs() < 1e-9);
        // 长句按时长 = 字符数 * 0.3
        let expected = "第二句比较长一点点".chars().count() as f64 * 0.3;
        assert!((segs[1].end - segs[1].start - expected).abs() < 1e-9);
    }

    /// BOM 与 CRLF 是 Windows 记事本另存的默认产物；不处理的话第一块会被当成
    /// 「带 BOM 的怪序号」而丢掉，或者时间戳行尾挂着 `\r` 解析失败。
    #[test]
    fn bom_and_crlf_are_handled() {
        let content = concat!(
            "\u{FEFF}1\r\n00:00:01,000 --> 00:00:02,000\r\n中文\r\n",
            "\r\n2\r\n00:00:02,000 --> 00:00:03,000\r\n第二\r\n",
        );
        let segs = parse_srt(content).unwrap();
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].text, "中文");
        assert_eq!(segs[1].text, "第二");

        // 纯 `\r`（老 Mac 风格）同样归一
        let cr_only = "1\r00:00:01,000 --> 00:00:02,000\r甲\r";
        let segs2 = parse_srt(cr_only).unwrap();
        assert_eq!(segs2.len(), 1);
        assert_eq!(segs2[0].text, "甲");
    }

    /// 扩展名判断不区分大小写（用户在资源管理器里看到的可能是 `.SRT`），
    /// 未知扩展名返回 `None`，由调用方决定提示语，而不是在这里猜格式。
    #[test]
    fn subtitle_format_from_path_is_case_insensitive() {
        assert_eq!(
            SubtitleFormat::from_path(Path::new("a.SRT")),
            Some(SubtitleFormat::Srt)
        );
        assert_eq!(
            SubtitleFormat::from_path(Path::new("b.Vtt")),
            Some(SubtitleFormat::Vtt)
        );
        assert_eq!(
            SubtitleFormat::from_path(Path::new("c.txt")),
            Some(SubtitleFormat::Txt)
        );
        assert_eq!(SubtitleFormat::from_path(Path::new("d.ass")), None);
        assert_eq!(SubtitleFormat::from_path(Path::new("noext")), None);
        assert_eq!(SubtitleFormat::from_path(Path::new("e.srt.bak")), None);
        assert_eq!(SubtitleFormat::Srt.label(), "SRT 字幕");
    }

    /// 中文文本必须逐字保真：解析路径上任何按字节切片/截断的实现都会在多字节
    /// UTF-8 上 panic 或把汉字切成乱码，这里用整句中文钉死。
    #[test]
    fn srt_preserves_chinese_text_exactly() {
        let text = "我们在这里讨论一个很长的中文句子，里面还有标点符号！";
        let content = format!("1\n00:00:01,000 --> 00:00:02,000\n{}\n", text);
        let segs = parse_srt(&content).unwrap();
        assert_eq!(segs[0].text, text);
        assert_eq!(segs[0].text.chars().count(), text.chars().count());
    }

    /// `parse` 必须按枚举分派到对应实现，不能把一种格式硬套到另一种上。
    #[test]
    fn parse_dispatches_by_format() {
        let srt = "1\n00:00:01,000 --> 00:00:02,000\nSRT 文本\n";
        let vtt = "WEBVTT\n\n00:00:03.000 --> 00:00:04.000\nVTT 文本\n";
        let txt = "纯文本一行\n";

        let a = parse(srt, SubtitleFormat::Srt).unwrap();
        assert_eq!(a[0].text, "SRT 文本");
        assert_eq!(a[0].start, 1.0);

        let b = parse(vtt, SubtitleFormat::Vtt).unwrap();
        assert_eq!(b[0].text, "VTT 文本");
        assert_eq!(b[0].start, 3.0);

        let c = parse(txt, SubtitleFormat::Txt).unwrap();
        assert_eq!(c[0].text, "纯文本一行");
        assert_eq!(c[0].start, 0.0);

        // 同一份 SRT 交给 TXT 解析会得到三行（序号/时间轴/文本），说明确实分派了
        assert_eq!(parse(srt, SubtitleFormat::Txt).unwrap().len(), 3);
        // 纯文本交给 SRT 解析没有时间轴，应当报错
        assert!(parse(txt, SubtitleFormat::Srt).is_err());
    }

    /// GBK 兜底：国内大量老字幕是 GBK 编码，UTF-8 解码会失败。这里真的写一个
    /// GBK 字节的临时文件，确认能读回正确中文而不是报错或乱码。
    #[test]
    fn read_subtitle_file_decodes_gbk_fallback() {
        let dir = temp_dir("gbk");
        let path = dir.join("gbk.srt");
        let content = concat!(
            "1\n00:00:01,000 --> 00:00:02,000\n第一句中文\n",
            "\n2\n00:00:02,000 --> 00:00:03,000\n第二句中文\n",
        );
        let (bytes, _, had_errors) = encoding_rs::GBK.encode(content);
        assert!(!had_errors, "测试样本必须能被 GBK 表示");
        std::fs::write(&path, &bytes[..]).unwrap();

        let segs = read_subtitle_file(&path).unwrap();
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].text, "第一句中文");
        assert_eq!(segs[1].text, "第二句中文");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// UTF-8 文件走主路径：不该被 GBK 兜底逻辑影响，中文要原样读出。
    #[test]
    fn read_subtitle_file_reads_utf8() {
        let dir = temp_dir("utf8");
        let path = dir.join("utf8.srt");
        let content = "1\n00:00:01,000 --> 00:00:02,000\n你好世界\n";
        std::fs::write(&path, content).unwrap();

        let segs = read_subtitle_file(&path).unwrap();
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].text, "你好世界");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 扩展名不认识时要在读盘前就报错，并带上路径方便用户定位问题文件。
    #[test]
    fn read_subtitle_file_rejects_unknown_extension() {
        let dir = temp_dir("badext");
        let path = dir.join("movie.ass");
        std::fs::write(&path, "whatever").unwrap();

        let err = read_subtitle_file(&path).unwrap_err().to_string();
        assert!(err.contains("movie.ass"), "错误信息应带路径: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
