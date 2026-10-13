//! 字幕导入：把 SRT / WebVTT / LRC / ASS(SSA) / TTML 系 / JSON / SBV / 纯文本读回 `Vec<Segment>`
//!
//! Voice2Word 能写出十余种字幕格式，导入侧却长期只认 SRT / WebVTT / 纯文本：用户手里
//! 有同事发来的 LRC 唱词、剪辑交付的 ASS，或者想修一版自己上次导出的 JSON / TTML 时，
//! 只能把整段视频重新转写一遍——又慢又费钱。本模块把导入侧补齐到与导出侧同一张格式表，
//! 导出什么格式就能原样读回什么格式。
//!
//! **各格式的时间信息完整度不同**，导入时必须区别对待，否则会把「猜出来的时间轴」
//! 当成文件里的真实标注：
//! - SRT / WebVTT / ASS / TTML / JSON / SBV 都带起止时间，读回的就是文件里的原始值；
//! - LRC 每行只带起点，中间各句的终点由**下一行的起点**补齐，只有最后一句的终点
//!   按字数估算（[`LRC_TAIL_SECONDS_PER_CHAR`]）；
//! - TXT 完全没有时间信息，整条时间轴都是按字数合成的占位值
//!   （[`TXT_SECONDS_PER_CHAR`]）。
//!
//! 后两种带估算成分，必须由调用方在导入提示里告诉用户（见 `src/ui/actions.rs`）。
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
    /// JSON 结构化字幕（本程序 `JsonSubtitleExporter` 的产物，含词级时间轴等全部字段）
    Json,
    /// LRC 歌词 / 唱词
    Lrc,
    /// ASS / SSA（Advanced SubStation Alpha）
    Ass,
    /// TTML 系（EBU-TT-D / Netflix TTAL / 通用 TTML、DFXP）
    Ttml,
    /// SBV（SubViewer）：`0:00:01.000,0:00:02.000` 一行时间轴 + 一行正文
    Sbv,
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
            "json" => Some(Self::Json),
            "lrc" => Some(Self::Lrc),
            // `ssa` 是 ASS 的旧称，正文结构（`[Events]` / `Dialogue:`）一致，同一实现读取。
            "ass" | "ssa" => Some(Self::Ass),
            // `dfxp` 是 TTML 的旧称；`ttal` 是 Netflix 的 TTML 变体，同样按 TTML 读取。
            "ttml" | "dfxp" | "ttal" => Some(Self::Ttml),
            "sbv" => Some(Self::Sbv),
            _ => None,
        }
    }

    /// 界面用的中文名，例如 "SRT 字幕"。
    pub fn label(self) -> &'static str {
        match self {
            Self::Srt => "SRT 字幕",
            Self::Vtt => "WebVTT 字幕",
            Self::Txt => "纯文本",
            Self::Json => "JSON 字幕",
            Self::Lrc => "LRC 歌词",
            Self::Ass => "ASS 字幕",
            Self::Ttml => "TTML 字幕",
            Self::Sbv => "SBV 字幕",
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
        // 关键字必须是**独立词**（后面跟空白或行尾）：`NOTE1` / `STYLE2` 这类是合法的
        // cue 标识行，按前缀匹配会把整条 cue 当注释丢掉。
        let head = first.split_whitespace().next().unwrap_or("");
        if ["NOTE", "STYLE", "REGION"]
            .iter()
            .any(|kw| head.eq_ignore_ascii_case(kw))
        {
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
        // 与 SRT 同口径：还原导出侧写出的实体（WebVTT 也走同一套转义）。
        let text = decode_markup_entities(&join_text_lines(&block[timing_at + 1..]));
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
        SubtitleFormat::Json => parse_json(content),
        SubtitleFormat::Lrc => parse_lrc(content),
        SubtitleFormat::Ass => parse_ass(content),
        SubtitleFormat::Ttml => parse_ttml(content),
        SubtitleFormat::Sbv => parse_sbv(content),
    }
}

/// 读文件并按扩展名解析。
///
/// **编码兜底**：先按 UTF-8 解码，失败则按 GBK 解码（中文字幕常见来源：老播放器
/// 导出、记事本另存、论坛下载的 SRT 都可能是 GBK），两条都解不干净时再用 lossy
/// 兜底：个别坏字节不该让整份字幕报废。
pub fn read_subtitle_file(path: &Path) -> Result<Vec<Segment>> {
    let format = SubtitleFormat::from_path(path).with_context(|| {
        format!(
            "不支持的字幕扩展名（只支持 .srt / .vtt / .txt / .json / .lrc / .ass / .ssa / .ttml / .dfxp / .ttal / .sbv）：{}",
            path.display()
        )
    })?;
    let bytes =
        std::fs::read(path).with_context(|| format!("读取字幕文件失败: {}", path.display()))?;
    let text = decode_subtitle_bytes(&bytes)?;
    parse(&text, format)
}

/// 编码兜底：先按 BOM 嗅探 UTF-16，再当 UTF-8 解，失败再当 GBK 解，都解不干净时
/// 用 lossy 兜底（坏字节变 U+FFFD），不整份报错。
///
/// Windows 记事本「Unicode」另存出来的字幕是 UTF-16LE/BE 带 BOM：UTF-8 解码必然
/// 失败，落到 GBK 分支又几乎必然 `had_errors`，于是报出「既不是 UTF-8 也不是 GBK」
/// 这种和实际不符的错误。先认 BOM 才能把这类文件正确读回。
fn decode_subtitle_bytes(bytes: &[u8]) -> Result<String> {
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return decode_utf16(rest, u16::from_le_bytes);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return decode_utf16(rest, u16::from_be_bytes);
    }
    if let Ok(text) = std::str::from_utf8(bytes) {
        return Ok(text.to_string());
    }
    let (text, _, had_errors) = encoding_rs::GBK.decode(bytes);
    if !had_errors {
        return Ok(text.into_owned());
    }
    // 严格 UTF-8 与 GBK 都没解干净：多半是某几处坏字节（拉丁编码、截断的下载文件、
    // 混入的二进制片段）。为几个坏字节放弃整份字幕代价太大，用 lossy 兜底把坏字节
    // 换成 U+FFFD，其余内容照常交给解析器——能救回多少是多少。
    Ok(String::from_utf8_lossy(bytes).into_owned())
}

/// 把去掉 BOM 的 UTF-16 负载按给定字节序解成字符串。
///
/// 奇数尾字节由 `as_chunks` 丢弃；孤立代理项会让 `from_utf16` 返回 `Err`，此时用
/// `from_utf16_lossy` 兜底成 U+FFFD，而不是 panic 或让整份字幕报错——个别坏字符不该
/// 毁掉整块内容。
fn decode_utf16(rest: &[u8], from_bytes: fn([u8; 2]) -> u16) -> Result<String> {
    let units: Vec<u16> = rest
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| from_bytes(*pair))
        .collect();
    Ok(String::from_utf16(&units).unwrap_or_else(|_| String::from_utf16_lossy(&units)))
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
    // 正文要还原导出侧写出的实体，保证「导出 → 导入 → 再导出」不逐层叠加转义。
    let text = decode_markup_entities(&join_text_lines(&block[timing_at + 1..]));
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
        // `MM:SS`：省略小时段。只有**秒**段必须 <60——分/时是累加单位，
        // WebVTT 允许 `90:00.000`（90 分）这类写法，浏览器原样接受。
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

/// 还原导出侧写进纯文本格式的实体（`&amp;` / `&lt;` / `&gt;`）。
///
/// 导出时 `&`/`<`/`>` 会被写成实体（见 writer 的 `escape_markup_entities`）：SRT/VTT/SBV
/// 的正文是纯文本，未转义的 `<` 会被播放器当标签吃掉。导入若不还原，用户重新导入
/// 自己导出的字幕就会看到字面量 `&amp;`，而且每经历一次「导出 → 导入 → 导出」就多叠
/// 一层（`a & b` → `a &amp; b` → `a &amp;amp; b`）。
///
/// **一遍扫字符**，与导出侧的单遍实现对称：产出的 `&` 不会再被回头识别，因此
/// `&amp;lt;` 只会还原成 `&lt;`（导出时的原文），不会被二次解码成 `<`。只认导出侧
/// 会产出的三个实体；其余 `&xxx;`（如 `&nbsp;`）原样保留，不猜。
fn decode_markup_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        if let Some(t) = tail.strip_prefix("&amp;") {
            out.push('&');
            rest = t;
        } else if let Some(t) = tail.strip_prefix("&lt;") {
            out.push('<');
            rest = t;
        } else if let Some(t) = tail.strip_prefix("&gt;") {
            out.push('>');
            rest = t;
        } else {
            out.push('&');
            rest = &tail[1..];
        }
    }
    out.push_str(rest);
    out
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

/// 解析 JSON 结构化字幕。
///
/// JSON 既是 Voice2Word 自己的导出格式（见 [`super::json::JsonSubtitleExporter`]），
/// 也是二次加工最常用的中间格式，因此导入必须与导出**往返无损**：
///
/// - 导出侧的 `raw_text` 写的是 [`Segment::display_text`]（优先润色文本），而
///   `text` 是按导出模式渲染后的结果（双语时含译文）。导入因此**优先取
///   `raw_text`**：它才是干净原文，拿 `text` 会把双语拼接或润色结果混进正文。
/// - 正文落在 `text`、`polished` 保持空，所以导入结果的 [`Segment::display_text`]
///   与导出前一致，再次导出 SRT/JSON 不会串行。
/// - `polished` 不是导出字段，往返后一律为空。这是**有意接受**的信息损失：
///   润色结果是本机产物，导出文件里没有它，也就无从恢复。
///
/// 文档结构收两种：导出器产出的根对象（取其中的 `segments`）与裸数组。裸数组是
/// 为了兼容手写 / 第三方工具产出的简化文档——它们通常直接给一个片段列表，没有
/// `format` / `version` 这些外壳，为此拒收整份文件没有意义。
///
/// 容错原则与 SRT/VTT 导入一致：单个坏块（时间戳缺失或倒挂、正文为空、元素根本
/// 不是对象）只丢它自己，后面的块照常解析；只有整篇一个可用块都没有时才由
/// [`finalize_segments`] 报错。
pub fn parse_json(content: &str) -> Result<Vec<Segment>> {
    let text = strip_bom(content);
    let doc: serde_json::Value =
        serde_json::from_str(text).with_context(|| "JSON 字幕解析失败：内容不是合法 JSON")?;

    // 根数组直接当片段列表用，根对象取 `segments`。其余情况（根是数字 / 字符串 /
    // 布尔 / null，或对象里没有数组型的 `segments`）属于「文档结构不对」，是前置
    // 错误：它与「结构没错但一块都解析不出来」对用户的含义完全不同，必须分开报。
    let items = match &doc {
        serde_json::Value::Array(items) => items,
        serde_json::Value::Object(map) => match map.get("segments") {
            Some(serde_json::Value::Array(items)) => items,
            _ => bail!(
                "JSON 字幕文档结构不对：根对象里缺少数组字段 \"segments\"\
                 （或该字段不是数组）"
            ),
        },
        _ => bail!("JSON 字幕文档结构不对：根节点既不是片段数组，也不是含 \"segments\" 的对象"),
    };

    let mut segments = Vec::new();
    for item in items {
        // 元素不是对象（数字 / 字符串 / null 等）时反序列化直接失败，按坏块跳过。
        let Ok(input) = serde_json::from_value::<JsonSegmentIn>(item.clone()) else {
            continue;
        };
        // 没有时间轴就无法定位，整块跳过。时间戳同时接受 JSON 数字与 `"00:00:01.500"`
        // 这类字符串（手写 / 第三方工具常见写法），统一交给 [`parse_timestamp`]，
        // 不为 JSON 另造一套时间解析；两种都取不到才判为坏块。
        let (Some(start), Some(end)) = (
            json_timestamp(input.start.as_ref()),
            json_timestamp(input.end.as_ref()),
        ) else {
            continue;
        };
        // 倒挂的时间轴（`end < start`）会让下游算出负时长，同样按坏块处理。
        if end < start {
            continue;
        }
        // 正文优先 `raw_text`；两者都取不到时说明这块没有可用内容，跳过。
        let Some(body) = pick_body_text(input.raw_text.as_ref(), input.text.as_ref()) else {
            continue;
        };

        // `index` 一律传 0：输入里的序号只当分块标记，最终由 [`finalize_segments`]
        // 按出现顺序重排为 1..n（手改过的 JSON 序号经常错乱甚至重复）。
        let mut seg = Segment::new(0, start, end, body);
        seg.translation = opt_body(input.translation.as_ref());
        seg.translation_lang = opt_identifier(input.translation_lang.as_ref());
        seg.language = opt_identifier(input.language.as_ref());
        seg.confidence = input.confidence.as_ref().and_then(|v| v.as_f64());
        // speaker 越界（超出 u32 上限）按「没有说话人信息」处理，不影响整块字幕。
        seg.speaker = input
            .speaker
            .as_ref()
            .and_then(|v| v.as_u64())
            .and_then(|n| u32::try_from(n).ok());
        // 词级时间戳是可选增强信息：缺失或形状不符时置空表，绝不因此丢掉整块字幕。
        seg.words = input
            .words
            .and_then(|v| serde_json::from_value::<Vec<super::segment::WordTiming>>(v).ok())
            .unwrap_or_default();
        // `polished` 保持 Segment::new 的空值：它不是导出字段，没有可恢复的来源。
        segments.push(seg);
    }
    finalize_segments(segments, "JSON")
}

/// JSON 片段的宽松中间结构体。
///
/// 字段一律用 [`serde_json::Value`] 承接，而不是直接声明
/// `Option<String>` / `Option<f64>` / `Option<u32>`。原因：导入规则要求「非字符串
/// 或纯空白 → None」「speaker 越界 → None」这类**逐字段降级**，而强类型字段一旦
/// 类型不符会让 `from_value` 整体失败，连时间轴与正文一起丢掉——一处元数据格式
/// 不对就报废整块字幕，代价太大。用 `Value` 承接后，单个字段坏掉只影响它自己。
/// 时间轴同样用 `Value` 承接：导出器写的是数字，但手写 / 第三方工具常写成
/// `"00:00:01.500"` 这样的字符串，交给 [`json_timestamp`] 两种都能认。
#[derive(serde::Deserialize)]
struct JsonSegmentIn {
    start: Option<serde_json::Value>,
    end: Option<serde_json::Value>,
    text: Option<serde_json::Value>,
    raw_text: Option<serde_json::Value>,
    translation: Option<serde_json::Value>,
    translation_lang: Option<serde_json::Value>,
    language: Option<serde_json::Value>,
    confidence: Option<serde_json::Value>,
    speaker: Option<serde_json::Value>,
    words: Option<serde_json::Value>,
}
/// 取正文：优先 `raw_text`，缺失 / 非字符串 / 纯空白时回退 `text`。
///
/// 两者都取不到时返回 `None`——没有文本的块既没有意义也无法呈现，直接跳过。
fn pick_body_text(
    raw_text: Option<&serde_json::Value>,
    text: Option<&serde_json::Value>,
) -> Option<String> {
    opt_body(raw_text).or_else(|| opt_body(text))
}

/// 取用户可见的文本（正文 / 译文）：非字符串或纯空白按缺失处理，其余**逐字保留**。
///
/// 只 `trim` 判空、不 `trim` 存值：正文里可能有刻意保留的换行与缩进，按字节裁剪
/// 会破坏中文（一个汉字占三字节，切在中间会 panic 或切出乱码）；而首尾空白一旦被
/// 吃掉，「导出 → 导入」的字段比对就会莫名其妙地不等。
fn opt_body(value: Option<&serde_json::Value>) -> Option<String> {
    let s = value?.as_str()?;
    if s.trim().is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// 取语言标识这类用于**精确比较**的短字符串，顺带去掉首尾空白。
///
/// 空白字符串按 `None` 而不是 `Some("")`：与 [`Segment::has_translation`] 的语义对齐
/// （纯空白不算「已翻译」）。若这里留下 `Some("")`，下游会把该段判成「已翻译」，
/// 质检面板显示 100% 而画面上却一片空白。语言标识同理去空白，否则 `" English "`
/// 会被 [`Segment::translation_matches`] 当成另一种语言而触发重译。
fn opt_identifier(value: Option<&serde_json::Value>) -> Option<String> {
    let s = value?.as_str()?.trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// 取时间戳：JSON 数字直接用，字符串交给 [`parse_timestamp`]。
///
/// 导出器写的是数字，但手写 / 第三方工具常把时间轴写成 `"00:00:01.500"` 这样的
/// 字符串；复用 [`parse_timestamp`] 而不是另造一套，两种来源的行为才能完全一致。
/// 其余类型（布尔 / 数组 / 对象 / null）按缺失处理，由调用方跳过该块。
fn json_timestamp(value: Option<&serde_json::Value>) -> Option<f64> {
    match value? {
        // 负时间戳会让下游算出负时长（播放器/剪辑软件直接崩），夹到 0 至少保住
        // 这一句的文本与结束时刻；非有限值（极端字面量）没法夹，按缺失处理。
        serde_json::Value::Number(n) => n.as_f64().filter(|v| v.is_finite()).map(|v| v.max(0.0)),
        serde_json::Value::String(s) => parse_timestamp(s),
        _ => None,
    }
}

/// LRC 唱词每字估算的演唱秒数（时间轴是**估算值**，不是文件里给出的）。
///
/// 与 [`TXT_SECONDS_PER_CHAR`] 同源：都是「文件里没有时间信息时按字数猜时长」的兜底
/// 策略，取值保持一致，用户校对时的心智模型才统一。
const LRC_TAIL_SECONDS_PER_CHAR: f64 = 0.3;
/// 最后一条唱词的最短估算时长（秒）。
///
/// 取 1.5 秒而不是纯文本的 1.0 秒：唱词一句通常比纯文本一行唱得久，下限过低会让尾句
/// 一闪而过，用户来不及看清。
const LRC_TAIL_MIN_DURATION: f64 = 1.5;

/// 解析 LRC 歌词文本。
///
/// 只认行首的 `[mm:ss.xx]` / `[mm:ss.xxx]` / `[mm:ss]` / `[hh:mm:ss.xx]` 时间标签。
/// `[ar:...]`、`[ti:...]`、`[al:...]`、`[by:...]`、`[offset:...]` 这类元数据标签解析
/// 不出时间，因此自然被忽略——不必另外维护一张元数据白名单：LRC 的标签名没有权威
/// 清单，白名单必然漏，靠「能不能解析成时间」判别反而不会误伤。
///
/// 每条唱词的结束时间取**下一条时间标签的起点**：LRC 只给起始时间，下一句开唱即上一句
/// 唱完，这是播放器通行的做法。最后一条没有「下一条」可依，只能按字数估算，因此调用方
/// 应当提示用户「时间轴末段为估算值，需要人工校对」。
pub fn parse_lrc(content: &str) -> Result<Vec<Segment>> {
    let text = normalize_newlines(strip_bom(content));
    // 先收集全部时间行（含空正文的「静音边界」），再统一组装：结束时间要看**下一条**，
    // 边扫边定 end 需要额外的状态机，分两步更直白，也不容易漏掉边界情况。
    let mut entries: Vec<(f64, String)> = Vec::new();
    for line in text.split('\n') {
        if let Some(entry) = parse_lrc_line(line) {
            entries.push(entry);
        }
    }
    let mut segments = Vec::new();
    for (i, (start, body)) in entries.iter().enumerate() {
        // 空正文的时间行只当「静音边界」用：它本身不是字幕，但仍是上一条结束时间的
        // 候选，因此不能从 entries 里删掉，否则上一句会一直唱到下一句开头。
        if body.is_empty() {
            continue;
        }
        let end = match entries.get(i + 1) {
            Some((next_start, _)) => *next_start,
            None => *start + lrc_tail_estimate(body),
        };
        // `end < start` 只丢这一条：时间倒挂多半是手改留下的，为它丢掉整篇等于让用户
        // 重做一遍导入，代价完全不成比例。
        if end < *start {
            continue;
        }
        segments.push(Segment::new(0, *start, end, body.clone()));
    }
    finalize_segments(segments, "LRC")
}

/// 解析一行 LRC，返回 `(起始秒, 正文)`；该行没有可用时间标签时返回 `None`。
fn parse_lrc_line(line: &str) -> Option<(f64, String)> {
    let mut rest = line.trim_start();
    // 时间标签必须在行首；行首不是 `[` 的行（纯歌词、说明文字、空行）整行跳过。
    if !rest.starts_with('[') {
        return None;
    }
    let mut start: Option<f64> = None;
    // 连续吃掉行首的所有 `[...]`：一行可以有多个时间标签（同一句副歌重复出现），但只取
    // 第一个能解析成时间的标签，其余标签只当分隔符。
    while let Some(after_open) = rest.strip_prefix('[') {
        // 没有配对的 `]`：这行结构已经坏了，整行作废，不要猜标签到哪儿结束。
        let close = after_open.find(']')?;
        if start.is_none() {
            start = parse_timestamp(&after_open[..close]);
        }
        rest = &after_open[close + 1..];
    }
    // 一个时间标签都没有（例如整行只有 `[ti:标题]`）：这行不是唱词。
    Some((start?, rest.trim().to_string()))
}

/// 估算最后一条唱词的时长（秒）。
///
/// 按**字符**数而不是字节数估算：一个汉字占 3 字节，按字节算会把中文尾句估成三倍长。
/// 这是估算值，不是文件里给出的真实时间。
fn lrc_tail_estimate(body: &str) -> f64 {
    (body.chars().count() as f64 * LRC_TAIL_SECONDS_PER_CHAR).max(LRC_TAIL_MIN_DURATION)
}

/// 解析 SBV（SubViewer）文本。
///
/// 结构是「一行时间轴 + 一行正文」交替，时间轴写作 `H:MM:SS.mmm,H:MM:SS.mmm`
/// （逗号分隔、毫秒用点号）。与 SRT 的差别只在时间戳写法与序号可有可无，因此
/// 坏块（时间戳不可解析 / `end < start` / 正文为空）的处理与 SRT 完全一致：
/// 丢它自己，不影响后面的块。
///
/// 时间戳的小时位可以不补零（`0:00:01.500`），`parse_timestamp` 已经吃下这种写法；
/// 只写 `M:SS.mmm,M:SS.mmm` 的两段式同样支持。
pub fn parse_sbv(content: &str) -> Result<Vec<Segment>> {
    let text = normalize_newlines(strip_bom(content));
    let mut segments = Vec::new();
    let mut pending: Option<Segment> = None;
    for line in text.split('\n') {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some((start, end)) = parse_sbv_timing_line(line) {
            // 上一条还没配上正文（时间轴行连着时间轴行）：它是坏块，直接丢掉，
            // 不要让它的时间被下面这条正文用上。
            pending = Some(Segment::new(0, start, end, String::new()));
            continue;
        }
        if let Some(mut seg) = pending.take() {
            // 正文行：无论上一条是否完整，都消费掉 pending。
            // 时间轴行丢了正文、或正文行没有对应的头，都只影响这一条。
            if !line.is_empty() && seg.end >= seg.start {
                // 与 SRT/VTT 同口径：还原导出侧写出的实体，避免反复导出叠加转义。
                seg.text = decode_markup_entities(line);
                segments.push(seg);
            }
        }
        // 没有 pending 的裸正文行（文件头部的说明文字）直接忽略。
    }
    finalize_segments(segments, "SBV")
}

/// 解析一行 SBV 时间轴：`start,end`。不是时间轴行（含正文或格式不符）返回 `None`。
fn parse_sbv_timing_line(line: &str) -> Option<(f64, f64)> {
    let (left, right) = line.split_once(',')?;
    // 时间轴行只有两个字段：多一个逗号说明这是正文（正文里逗号极常见），
    // 因此字段数不对就整行当正文，而不是硬解前半段。
    if line.matches(',').count() != 1 {
        return None;
    }
    // 两侧都必须含 `:`（SBV 的时间至少是 `M:SS.mmm`）：否则形如 `10,20` / `1,000`
    // 的**正文行**会被误判成时间轴，把上一条 pending 覆盖掉、整块内容错位或消失。
    if !left.contains(':') || !right.contains(':') {
        return None;
    }
    let start = parse_timestamp(left)?;
    let end = parse_timestamp(right)?;
    if end < start {
        return None;
    }
    Some((start, end))
}
/// 解析 ASS / SSA 字幕文本。
///
/// 只读 `[Events]` 段里的 `Dialogue:` 行：`[Script Info]`、`[V4+ Styles]`、`[Fonts]`
/// 等段落里也可能出现形如 `Dialogue:` 的行（注释模板、示例行），把它们当字幕会凭空多出
/// 内容，因此段名不匹配就立刻停止收集，直到下一个 `[...]` 段名。
pub fn parse_ass(content: &str) -> Result<Vec<Segment>> {
    let text = normalize_newlines(strip_bom(content));
    let mut segments = Vec::new();
    let mut in_events = false;
    // 说话人编号全篇共用一张表：同一名字无论出现多少次都必须拿到同一个编号，否则下游按
    // 编号分色/分组时会把人拆成好几个。
    let mut seen_speakers: Vec<String> = Vec::new();
    for line in text.split('\n') {
        let line = line.trim();
        // 段名行：`[Events]` 进入事件段，任何其它 `[...]` 都退出事件段。
        if let Some(rest) = line.strip_prefix('[') {
            let name = match rest.split_once(']') {
                Some((name, _)) => name,
                None => rest,
            };
            in_events = name.trim().eq_ignore_ascii_case("Events");
            continue;
        }
        if !in_events {
            continue;
        }
        // 段内只认 `Dialogue:`（大小写不敏感，前导空白已由 trim 去掉）；
        // `Comment:`、`Format:`、`Picture:` 等一律跳过。
        let Some(fields_text) = strip_prefix_ignore_ascii_case(line, "Dialogue:") else {
            continue;
        };
        // 字段顺序固定为 Layer,Start,End,Style,Name,MarginL,MarginR,MarginV,Effect,Text。
        // 必须用 `splitn(10, ...)`：Text 里可以有逗号，靠 limit 把剩余内容整段留给最后
        // 一个字段，否则文本会在第一个逗号处被截断。
        let fields: Vec<&str> = fields_text.splitn(10, ',').collect();
        if fields.len() < 10 {
            continue; // 字段数不足是坏行，丢掉它但不影响后面的行
        }
        let Some(start) = parse_timestamp(fields[1].trim()) else {
            continue;
        };
        let Some(end) = parse_timestamp(fields[2].trim()) else {
            continue;
        };
        if end < start {
            continue;
        }
        // 先剥 `{...}` 覆盖码、再还原 `\N` 之类的标记：顺序不能反——覆盖码里也可能出现
        // `\N`，先还原就会把排版指令误当换行。
        let decoded = decode_ass_text_markers(&strip_ass_override_blocks(fields[9]));
        let body = decoded.trim();
        if body.is_empty() {
            continue;
        }
        // 说话人启发式：ASS 的 Name 字段是演员/说话人名，第三方工具常填真实人名。但大量
        // 文件（包括本项目的导出侧）全篇写 `Default` 或留空，此时强行加标签只会给每句挂
        // 上无意义的「说话人 1」，因此这两种情况一律视为没有说话人。
        let name = fields[4].trim();
        let speaker = if name.is_empty() || name.eq_ignore_ascii_case("Default") {
            None
        } else {
            Some(ass_speaker_index(&mut seen_speakers, name))
        };
        let mut seg = Segment::new(0, start, end, body);
        seg.speaker = speaker;
        segments.push(seg);
    }
    finalize_segments(segments, "ASS")
}

/// 剥掉 ASS 的 `{...}` 覆盖标签，返回剩下的正文字符串。
///
/// `{` 到 `}` 之间的内容（`{\i1}`、`{\pos(10,10)}`）是排版指令，不是字幕文本。未闭合的
/// `{` 之后的整段一并丢弃：既然分不清指令到哪里结束，留下来只会把半截指令混进字幕，
/// 宁可让这一行少一截，也不污染文本。
///
/// `\{` / `\}` / `\\` 是转义序列（导出侧正是用 `\{` / `\}` 表达字面花括号），必须
/// 逐个跳过、不能触发覆盖块状态：否则 `x\{a\}b` 会被当成「`{` 开、`}` 关」的覆盖块，
/// 把 `a` 连同转义前缀一起吞掉，读回来只剩 `x\b`。
fn strip_ass_override_blocks(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut in_brace = false;
    let mut chars = raw.chars().peekable();
    while let Some(ch) = chars.next() {
        // 覆盖块之外的 `\`：`\{` / `\}` / `\\` 是导出侧写出的**字面量**（本例的
        // 转义规则），原样留给 [`decode_ass_text_markers`] 还原。绝不能让 `\{` 里的
        // `{` 打开覆盖块——否则它后面直到下一个 `}` 的正文会被整段吞掉。
        if ch == '\\' && !in_brace {
            out.push('\\');
            if let Some(&next) = chars.peek() {
                if matches!(next, '{' | '}' | '\\') {
                    out.push(next);
                    chars.next();
                }
            }
            continue;
        }
        match ch {
            '{' => in_brace = true,
            '}' if in_brace => in_brace = false,
            _ if in_brace => {}
            _ => out.push(ch),
        }
    }
    out
}

/// 把 ASS 的换行/空格标记还原成真实字符。
///
/// `\N` 是 ASS 的硬换行。`\n` 是同一语义的旧写法（SSA 时代起两种都收），本项目的导出侧
/// 也正是用 `\N` 表达双语/折行的换行；只认 `\N` 的话，含 `\n` 的文件会把同一句的上下两
/// 行拼成一长行，与导出语义对不上。`\h` 是不换行的硬空格（常用来保住行首空格），还原成
/// 普通空格。`\{` / `\}` / `\\` 还原成对应的单个字面字符——它们与导出侧
/// [`strip_ass_override_blocks`] 的转义互为逆运算。其余 `\x` 原样保留：不认识的转义
/// 宁可留着，也不要凭猜测删掉用户原文里的字符。
fn decode_ass_text_markers(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('N' | 'n') => out.push('\n'),
            // 只认小写 `\h`：ASS 规范里的硬空格就是 `\h`，大写 `\H` 不是合法标记，
            // 因此按未知转义原样保留，不凭猜测转换。
            Some('h') => out.push(' '),
            // `\{` / `\}` / `\\` 是导出侧对字面花括号与反斜杠的转义，逐个还原；
            // 不还原的话，导出再导入会把正文里的 `{`/`\` 越转越多。
            Some(escaped @ ('{' | '}' | '\\')) => out.push(escaped),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// 大小写不敏感地剥掉 ASCII 前缀；不匹配返回 `None`。
///
/// 用 `str::get` 取前缀而不是直接按字节切片：前缀长度可能落在多字节字符中间，
/// `&line[..n]` 会 panic，而 ASS 文件里出现中文行完全正常。
fn strip_prefix_ignore_ascii_case<'a>(line: &'a str, prefix: &str) -> Option<&'a str> {
    let head = line.get(..prefix.len())?;
    if head.eq_ignore_ascii_case(prefix) {
        Some(&line[prefix.len()..])
    } else {
        None
    }
}

/// 取说话人编号：首次出现的名字依次编号 0、1、2…，重复出现复用原编号。
fn ass_speaker_index(seen: &mut Vec<String>, name: &str) -> u32 {
    match seen.iter().position(|n| n == name) {
        Some(pos) => pos as u32,
        None => {
            seen.push(name.to_string());
            (seen.len() - 1) as u32
        }
    }
}

/// 解析 TTML 系字幕（EBU-TT-D / Netflix TTAL / 通用 TTML）。
///
/// # 为什么手写扫描器而不是引入 XML 依赖
///
/// 本项目只依赖 `regex` / `serde_json`，没有 XML 解析库（见 `Cargo.toml`）。为一个
/// 「读回自家导出文件」的次要功能引入完整 XML 依赖并不划算：依赖体积、编译时间与
/// 供应链面都会随之上涨。而这里要识别的结构极其有限——根语言、`<p>` 时间轴、
/// `<span xml:lang>` 语言切分、`<br/>` 换行与五个预定义实体——手写一个「容错优先」
/// 的小状态机反而更贴合需求。
///
/// # 为什么不用正则
///
/// 正文里可能同时出现属性引号、被转义的 `&lt;p&gt;` 与真实标签，正则很难在不回溯
/// 爆炸的前提下正确跳过引号内的 `>`。逐字符扫描只有「在不在引号里」一个状态，
/// 实现与验证成本都低得多。
///
/// # 容错原则
///
/// 与 SRT / VTT 一致：单个 `<p>` 的时间轴缺失、解析失败或 `end < begin` 只丢那一块，
/// 继续处理后面的块；只有整篇一个可用块都没有时，才由 [`finalize_segments`] 报错。
///
/// # 为什么 `.xml` 不进 `SubtitleFormat::from_path`
///
/// `.xml` 是通用后缀，Final Cut / Premiere 工程与普通数据文件都可能用它，按后缀就
/// 认定是字幕会造成「导入成功但内容全乱」。安全集合是字幕专有的 `.ttml` / `.dfxp` /
/// `.ttal`；扩展名分派由 `reader.rs` 统一维护，本文件只提供纯字符串解析。
pub fn parse_ttml(content: &str) -> Result<Vec<Segment>> {
    let text = strip_xml_comments(&normalize_newlines(strip_bom(content)));
    let doc_lang = ttml_document_lang(&text);
    let mut segments = Vec::new();
    // 字节游标顺序推进：每处理完一块就跳过它的 `</p>`，避免把同一块重复匹配。
    let mut cursor = 0usize;
    while let Some(at) = find_element_start(&text, cursor, "p") {
        let Some((attrs, after_open, self_closing)) = scan_start_tag(&text, at, "p") else {
            // 标签始终没有闭合：文档已被截断，后面不可能再有完整块。
            break;
        };
        // `<p ... />` 没有正文，跳过即可；不能拿它的后续内容去找 `</p>`。
        if self_closing {
            cursor = after_open;
            continue;
        }
        let Some((close_at, after_close)) = find_close_tag(&text, after_open, "p") else {
            break;
        };
        let body = &text[after_open..close_at];
        cursor = after_close;
        if let Some(segment) = build_ttml_segment(&attrs, body, doc_lang.as_deref()) {
            segments.push(segment);
        }
    }
    finalize_segments(segments, "TTML")
}

/// 去掉 XML 注释（`<!-- ... -->`）。
///
/// 注释里被「注掉」的 `<p>...</p>` 仍是合法字节序列：不剥掉的话，注释掉的字幕会被
/// 当成真字幕解析出来，凭空多出几句、还可能与正文重复。未闭合的注释视为文档在此
/// 截断，后面的内容不再当正文。
fn strip_xml_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("<!--") {
        out.push_str(&rest[..at]);
        let after = &rest[at + "<!--".len()..];
        match after.find("-->") {
            Some(end) => rest = &after[end + "-->".len()..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

/// 在 `text[from..]` 中寻找名为 `name` 的**开**标签，返回 `<` 的字节下标。
///
/// 为什么要求名字后面是空白 / `>` / `/`：否则找 `<p` 时会误命中 `<person>`、
/// `<publisher>` 这类同前缀元素，把无关内容当成字幕块。
fn find_element_start(text: &str, from: usize, name: &str) -> Option<usize> {
    let mut cursor = from;
    while let Some(rel) = text[cursor..].find('<') {
        let at = cursor + rel;
        if let Some(name_at) = local_name_start(text, at + 1, name) {
            let boundary = text[name_at + name.len()..].chars().next();
            let is_boundary = match boundary {
                None => true,
                Some(c) => c.is_whitespace() || c == '>' || c == '/',
            };
            if is_boundary {
                return Some(at);
            }
        }
        cursor = at + 1;
    }
    None
}

/// 元素**本地名**的起始下标：允许可选的命名空间前缀（`<tt:p>`、`<ns0:span>`）。
///
/// 找不到返回 `None`。前缀必须紧贴 `:` 且其后就是目标元素名，否则 `<person>` 会被
/// 误当成 `<p>` 的同前缀元素。
fn local_name_start(text: &str, from: usize, name: &str) -> Option<usize> {
    if let Some(rest) = text[from..].strip_prefix(name) {
        // 名字后紧跟 `:` 说明这里是**前缀**位置（`<tt:tt>` 里的第一个 `tt`），
        // 不是本地名；交给下面的前缀分支从 `:` 之后再匹配一次。
        if !rest.starts_with(':') {
            return Some(from);
        }
    }
    let mut idx = from;
    let mut has_prefix = false;
    for c in text[from..].chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
            idx += c.len_utf8();
            has_prefix = true;
            continue;
        }
        break;
    }
    if has_prefix && text[idx..].starts_with(':') {
        let after = idx + 1;
        if text[after..].starts_with(name) {
            return Some(after);
        }
    }
    None
}

/// 寻找闭合标签 `</name>`：允许名字后带空白（`</p >` 是合法 XML），也允许带命名
/// 空间前缀（`</tt:p>`）。返回 `(闭合标签起始下标, 标签结束之后的下标)`。
///
/// 必须容忍这两种写法：硬编码 `</p>` 时，`</p >` 会让查找失败，调用方据此判定
/// 「文档截断」而**丢掉该块及其后的所有块**。
fn find_close_tag(text: &str, from: usize, name: &str) -> Option<(usize, usize)> {
    let mut cursor = from;
    while let Some(rel) = text[cursor..].find("</") {
        let at = cursor + rel;
        if let Some(name_at) = local_name_start(text, at + 2, name) {
            let mut idx = name_at + name.len();
            while let Some(c) = text[idx..].chars().next() {
                if c == '>' {
                    return Some((at, idx + 1));
                }
                if !c.is_whitespace() {
                    break;
                }
                idx += c.len_utf8();
            }
        }
        cursor = at + 2;
    }
    None
}

/// 扫描 `tag_at` 处的开标签，返回 `(属性串, 标签结束后的下标, 是否自闭合)`。
///
/// 引号内的 `>` 不能当作标签结束：属性值里出现 `>`（如某些 style 表达式）时，
/// 若不做引号状态判断，标签会被从中间截断，后面所有块的解析跟着错位。
fn scan_start_tag(text: &str, tag_at: usize, name: &str) -> Option<(String, usize, bool)> {
    // 命名空间前缀由 [`local_name_start`] 跳过：`<tt:p ...>` 的属性串同样从元素名之后算起。
    let content_start = local_name_start(text, tag_at + 1, name)? + name.len();
    let mut quote: Option<char> = None;
    for (offset, ch) in text[content_start..].char_indices() {
        match ch {
            '"' | '\'' => {
                if quote == Some(ch) {
                    quote = None;
                } else if quote.is_none() {
                    quote = Some(ch);
                }
            }
            '>' if quote.is_none() => {
                let attrs = &text[content_start..content_start + offset];
                let self_closing = attrs.trim_end().ends_with('/');
                return Some((attrs.to_string(), content_start + offset + 1, self_closing));
            }
            _ => {}
        }
    }
    None
}

/// 从属性串里取名为 `name` 的属性值（去引号）。找不到返回 `None`。
///
/// 只认带引号的值：TTML 的属性值规范上必须加引号，放宽到无引号会带来歧义
/// （空值与下一个属性名都可能被吞进去），不如直接判为缺失。
fn ttml_attr(attrs: &str, name: &str) -> Option<String> {
    let mut search = 0;
    while let Some(rel) = attrs[search..].find(name) {
        let at = search + rel;
        let after = at + name.len();
        search = after;
        // 名字必须是独立词：前面是串首或空白，否则可能落在别的属性名/属性值里。
        let boundary_ok = match attrs[..at].chars().next_back() {
            None => true,
            Some(c) => c.is_whitespace(),
        };
        if !boundary_ok {
            continue;
        }
        let Some(rest) = attrs[after..].trim_start().strip_prefix('=') else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(quote @ ('"' | '\'')) = rest.chars().next() else {
            continue;
        };
        let qlen = quote.len_utf8();
        if let Some(end) = rest[qlen..].find(quote) {
            return Some(rest[qlen..qlen + end].to_string());
        }
    }
    None
}

/// 取根 `<tt>` 上的 `xml:lang`（文档主语言），空值视为未声明。
///
/// 文档语言是双语切分的基准：它决定「哪种语言算原文、哪种算译文」。缺失时
/// [`ttml_split_bilingual`] 会退化为「以第一个带语言的 span 为原文」。
fn ttml_document_lang(text: &str) -> Option<String> {
    let at = find_element_start(text, 0, "tt")?;
    let (attrs, _, _) = scan_start_tag(text, at, "tt")?;
    let lang = ttml_attr(&attrs, "xml:lang")?;
    let lang = lang.trim();
    if lang.is_empty() {
        None
    } else {
        Some(lang.to_string())
    }
}

/// 把 TTML 时间表达式解析成秒。
///
/// 先交给 [`parse_timestamp`]（覆盖 `HH:MM:SS.mmm` 时钟时间），再兜底处理 TTML
/// 特有的「数字 + 单位」偏移写法：`ms` / `h` / `m` / `s`。`ms` 必须排在 `s` 之前
/// 匹配，否则 `500ms` 会先被当成 `500s`。
///
/// 帧式时间（`10f`）刻意不支持：换算需要根元素上的 `ttp:frameRate`，而该属性可能
/// 缺失或与内容不符，猜错帧率会让整条时间轴静默偏移，不如按坏块跳过、由用户显式
/// 修正。这里因此不写任何帧率默认值。
fn parse_ttml_time(raw: &str) -> Option<f64> {
    let text = raw.trim();
    if text.is_empty() {
        return None;
    }
    if let Some(seconds) = parse_timestamp(text) {
        return Some(seconds);
    }
    let lower = text.to_ascii_lowercase();
    for (unit, scale) in [("ms", 0.001), ("h", 3600.0), ("m", 60.0), ("s", 1.0)] {
        let Some(number) = lower.strip_suffix(unit) else {
            continue;
        };
        let number = number.trim();
        if number.is_empty() {
            continue;
        }
        // 只接受「数字 + 至多一个小数点」，挡住 `1h2m` 这类组合写法。
        let mut seen_dot = false;
        let mut digits_only = true;
        for b in number.bytes() {
            match b {
                b'0'..=b'9' => {}
                b'.' if !seen_dot => seen_dot = true,
                _ => {
                    digits_only = false;
                    break;
                }
            }
        }
        if !digits_only {
            continue;
        }
        if let Ok(value) = number.parse::<f64>() {
            let scaled = value * scale;
            // 位数极多的数字会溢出成 inf：放它进去会让整条时间轴变成 inf，
            // 时长/导出全部失效。此时按坏块处理，不返回这个值。
            if scaled.is_finite() {
                return Some(scaled);
            }
            continue;
        }
    }
    None
}

/// 由 `<p>` 的属性与正文构造一条 `Segment`；缺时间轴或正文为空时返回 `None`。
///
/// 结束时间优先取 `end`，没有 `end` 时用 `begin + dur` 兜底——两种写法在 TTML
/// 里都常见，只认其中一种会让另一半文件整篇失败。
fn build_ttml_segment(attrs: &str, body: &str, doc_lang: Option<&str>) -> Option<Segment> {
    let begin = parse_ttml_time(&ttml_attr(attrs, "begin")?)?;
    let end = match ttml_attr(attrs, "end") {
        Some(raw) => parse_ttml_time(&raw)?,
        None => {
            let dur = parse_ttml_time(&ttml_attr(attrs, "dur")?)?;
            begin + dur
        }
    };
    // 非有限时间（位数极多的偏移量算出 inf，或 begin + dur 溢出）会让整条时间轴崩掉，
    // 按坏块丢弃。
    if !begin.is_finite() || !end.is_finite() {
        return None;
    }
    // `end < begin` 的倒挂块必须丢：留着会让播放器算出负时长。
    if end < begin {
        return None;
    }
    let (spans, bare) = ttml_inline_pieces(body);
    let (original, translation) = ttml_split_bilingual(&spans, &bare, doc_lang);
    let text = ttml_normalize_text(&original);
    if text.is_empty() {
        return None;
    }
    let mut segment = Segment::new(0, begin, end, text);
    let translation = translation.and_then(|(trans, lang)| {
        let trans = ttml_normalize_text(&trans);
        (!trans.is_empty()).then_some((trans, lang))
    });
    if let Some((trans, lang)) = translation {
        segment.translation = Some(trans);
        segment.translation_lang = lang;
    }
    segment.speaker = ttml_attr(attrs, "ttm:agent")
        .as_deref()
        .and_then(parse_ttml_speaker);
    Some(segment)
}

/// `ttm:agent` 的 `xml:id` → 说话人编号（0 起）。
///
/// 与导出侧 `speaker_agent_id` 的 `spk{N}`（N 从 1 起）互为逆运算：`spk1` → `0`。
/// 非 `spk` 前缀或编号为 0 的 id（如 `narrator`、`spk0`）一律不映射，宁可让说话人
/// 留空，也不要凭猜测把旁白错归给某个人。
fn parse_ttml_speaker(id: &str) -> Option<u32> {
    let digits = id.strip_prefix("spk")?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse::<u32>().ok()?.checked_sub(1)
}

/// 拆出 `<p>` 正文里的语言 span 与裸文本，返回 `(spans, bare)`。
///
/// `spans` 每项是 `(span 上的 xml:lang, 文本)`，`bare` 是所有不在 span 内的文本
/// （含 `<br/>` 产生的换行）。相邻且语言相同的 span 会合并：karaoke 逐字切分或
/// 分段样式会产生大量同语言碎片，逐片成段会把一句话拆成好几条字幕。
///
/// 未闭合的 `<span>` 视为文档截断，把剩余内容降级为裸文本而不是整段丢弃。
fn ttml_inline_pieces(body: &str) -> (Vec<(Option<String>, String)>, String) {
    let mut spans: Vec<(Option<String>, String)> = Vec::new();
    let mut bare = String::new();
    let mut cursor = 0usize;
    while let Some(at) = find_element_start(body, cursor, "span") {
        let Some((attrs, after_open, self_closing)) = scan_start_tag(body, at, "span") else {
            break;
        };
        // 开标签之前的裸文本先归入裸文本缓冲。
        bare.push_str(&ttml_inline_text(&body[cursor..at]));
        cursor = after_open;
        if self_closing {
            continue;
        }
        let Some((inner_at, after_close)) = find_close_tag(body, cursor, "span") else {
            bare.push_str(&ttml_inline_text(&body[cursor..]));
            cursor = body.len();
            break;
        };
        let inner = &body[cursor..inner_at];
        cursor = after_close;
        let lang = ttml_attr(&attrs, "xml:lang")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let text = ttml_inline_text(inner);
        let merge_last = spans
            .last()
            .is_some_and(|(last_lang, _)| *last_lang == lang);
        if merge_last {
            if let Some((_, last_text)) = spans.last_mut() {
                last_text.push_str(&text);
            }
            continue;
        }
        spans.push((lang, text));
    }
    bare.push_str(&ttml_inline_text(&body[cursor..]));
    (spans, bare)
}

/// span 的有效语言：span 自己声明了就用它，否则继承文档语言。
///
/// 继承是 TTML 语义的一部分——`<span>` 不写 `xml:lang` 就表示与文档同语言，
/// 不继承会把「未标注」错当成「另一种语言」，从而凭空生成译文。
fn ttml_effective_lang<'a>(
    span_lang: &'a Option<String>,
    doc_lang: Option<&'a str>,
) -> Option<&'a str> {
    span_lang.as_deref().or(doc_lang)
}

/// 把 `(spans, bare)` 切成「原文 + 可选译文」。
///
/// 基准语言取文档语言；文档没声明时退化为第一个带语言的 span（Netflix TTAL 常见
/// 写法）。原文优先取「有效语言 == 基准语言」的第一个非空 span，其次是裸文本，最后
/// 才兜底用第一个非空 span；译文取第一个「有效语言 != 基准语言」的非空 span，语言
/// 标记原样带出。这样单语文件不会凭空长出译文，双语文件也不依赖 span 的先后顺序。
fn ttml_split_bilingual(
    spans: &[(Option<String>, String)],
    bare: &str,
    doc_lang: Option<&str>,
) -> (String, Option<(String, Option<String>)>) {
    let base = doc_lang.or_else(|| spans.iter().find_map(|(lang, _)| lang.as_deref()));
    let original = spans
        .iter()
        .find(|(lang, text)| !text.is_empty() && ttml_effective_lang(lang, doc_lang) == base)
        .map(|(_, text)| text.clone())
        .or_else(|| (!bare.is_empty()).then_some(bare.to_string()))
        .or_else(|| {
            spans
                .iter()
                .find(|(_, text)| !text.is_empty())
                .map(|(_, text)| text.clone())
        })
        .unwrap_or_default();
    let translation = spans
        .iter()
        .find(|(lang, text)| !text.is_empty() && ttml_effective_lang(lang, doc_lang) != base)
        .map(|(lang, text)| (text.clone(), lang.clone()));
    (original, translation)
}

/// 归一化一条字幕文本：逐行去首尾空白、丢掉空行、用 `\n` 重新拼接。
///
/// 为什么按行 trim：`<p>` 的源码缩进换行与 `<br/>` 产生的换行在字符串层面无法区分，
/// 保留缩进会导出「　你好」这类带前导空格的字幕。代价是行内刻意的首尾空格会丢失，
/// 这在字幕场景可以接受（首尾空格本就不该出现在画面里）。
fn ttml_normalize_text(raw: &str) -> String {
    raw.split('\n')
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// 把一段内联标记转成纯文本：解实体、`<br/>` 变 `\n`、其余标签剥掉。
fn ttml_inline_text(raw: &str) -> String {
    let mut out = String::new();
    let mut rest = raw;
    while let Some(lt) = rest.find('<') {
        out.push_str(&decode_xml_entities(&rest[..lt]));
        let after = &rest[lt + 1..];
        // `<![CDATA[...]]>`：内容按**字面文本**取用，不解实体、不剥标签。部分 TTML
        // 导出器用它包正文；不认的话整段（含其中的 `<`）会被当标签吃掉，字幕直接空掉。
        if let Some(cdata) = after.strip_prefix("![CDATA[") {
            match cdata.find("]]>") {
                Some(end) => {
                    out.push_str(&cdata[..end]);
                    rest = &cdata[end + "]]>".len()..];
                    continue;
                }
                None => {
                    // 未闭合的 CDATA：把剩余内容当文本，避免整句凭空消失。
                    out.push_str(cdata);
                    return out;
                }
            }
        }
        let Some(gt) = after.find('>') else {
            // 没有闭合的 `<`：文档已被截断，余下内容不再当正文，避免半截标记写进字幕。
            return out;
        };
        if is_br_tag(&after[..gt]) {
            out.push('\n');
        }
        rest = &after[gt + 1..];
    }
    out.push_str(&decode_xml_entities(rest));
    out
}

/// 判断标签名是否为 `br`（允许 `br/`、`BR`、`br /` 等写法）。
fn is_br_tag(tag: &str) -> bool {
    let tag = tag.trim();
    let tag = tag.strip_suffix('/').unwrap_or(tag).trim_end();
    tag.eq_ignore_ascii_case("br")
}

/// 解码 XML 预定义实体与数字字符引用，未知实体原样保留。
///
/// 未知实体（如 `&nbsp;`）保留原文而不是丢弃：TTML 只预定义了五个实体，保留能让
/// 用户看到真实输入，也避免「解错比不解更糟」。数字引用支持十进制 `&#20013;` 与
/// 十六进制 `&#x4E2D;`，越界码点同样原样保留。
fn decode_xml_entities(raw: &str) -> String {
    if !raw.contains('&') {
        return raw.to_string();
    }
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let tail = &rest[amp + 1..];
        let decoded = tail
            .find(';')
            .and_then(|semi| decode_entity(&tail[..semi]).map(|ch| (semi, ch)));
        if let Some((semi, ch)) = decoded {
            out.push(ch);
            rest = &tail[semi + 1..];
            continue;
        }
        // 未知实体或缺少 `;`：原样保留 `&`，从 `&` 之后继续扫描。
        out.push('&');
        rest = tail;
    }
    out.push_str(rest);
    out
}

/// 解码单个实体名（`&` 与 `;` 之间、不含两端）。无法识别返回 `None`。
fn decode_entity(name: &str) -> Option<char> {
    match name {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        _ => decode_numeric_entity(name),
    }
}

/// 解码数字字符引用（`#123` / `#x4E2D`）；非数字引用返回 `None`。
fn decode_numeric_entity(name: &str) -> Option<char> {
    let digits = name.strip_prefix('#')?;
    let (radix, digits) = match digits
        .strip_prefix('x')
        .or_else(|| digits.strip_prefix('X'))
    {
        Some(hex) => (16u32, hex),
        None => (10u32, digits),
    };
    if digits.is_empty() {
        return None;
    }
    let code = u32::from_str_radix(digits, radix).ok()?;
    char::from_u32(code)
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

    /// 无小时段时**只有秒**受 60 限制：WebVTT 允许 `90:00.000`（90 分）这类写法，
    /// 分钟段是累加单位本就可以溢出。这条把这个口径钉住——若哪天有人把 `[m,s]`
    /// 分支改成照抄 `[h,m,s]` 的 `m >= 60`，1 小时以上的 WebVTT 会被静默丢块。
    #[test]
    fn mm_ss_allows_minutes_over_59_but_not_seconds() {
        let vtt = "WEBVTT\n\n90:00.000 --> 90:02.500\n长片\n";
        let segs = parse_vtt(vtt).unwrap();
        assert_eq!(segs.len(), 1, "MM 超过 59 仍需解析");
        assert_eq!(segs[0].start, 5400.0);
        assert_eq!(segs[0].end, 5402.5);
        // 秒段 >=60 依然是坏时间戳（MM:SS 里的 SS 不能溢出）
        let bad = "WEBVTT\n\n01:90.000 --> 01:95.000\n越界\n";
        assert!(parse_vtt(bad).is_err(), "秒段 >=60 必须判坏");
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
        assert_eq!(
            SubtitleFormat::from_path(Path::new("d.json")),
            Some(SubtitleFormat::Json)
        );
        assert_eq!(
            SubtitleFormat::from_path(Path::new("e.LRC")),
            Some(SubtitleFormat::Lrc)
        );
        // `ass` 与旧称 `ssa` 走同一实现；`ttml` 的旧称 `dfxp` 与 Netflix 变体 `ttal` 同理。
        assert_eq!(
            SubtitleFormat::from_path(Path::new("f.ass")),
            Some(SubtitleFormat::Ass)
        );
        assert_eq!(
            SubtitleFormat::from_path(Path::new("g.SSA")),
            Some(SubtitleFormat::Ass)
        );
        assert_eq!(
            SubtitleFormat::from_path(Path::new("h.ttml")),
            Some(SubtitleFormat::Ttml)
        );
        assert_eq!(
            SubtitleFormat::from_path(Path::new("i.DFXP")),
            Some(SubtitleFormat::Ttml)
        );
        assert_eq!(
            SubtitleFormat::from_path(Path::new("j.ttal")),
            Some(SubtitleFormat::Ttml)
        );
        assert_eq!(SubtitleFormat::from_path(Path::new("noext")), None);
        assert_eq!(SubtitleFormat::from_path(Path::new("k.srt.bak")), None);
        assert_eq!(SubtitleFormat::Srt.label(), "SRT 字幕");
        assert_eq!(SubtitleFormat::Lrc.label(), "LRC 歌词");
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

        // 新增的四种格式同样必须走各自的实现：各给一份最小样本，文本与起点都要对得上。
        // 这条断言钉的是「枚举加了、分派漏了」这种静默错误——它不会报错，只会让用户
        // 拿到一份格式套错、内容全乱的工程。
        let json = r#"{"segments":[{"start":5.0,"end":6.0,"text":"JSON 文本"}]}"#;
        let d = parse(json, SubtitleFormat::Json).unwrap();
        assert_eq!(d[0].text, "JSON 文本");
        assert_eq!(d[0].start, 5.0);

        let lrc = "[00:07.00]LRC 文本\n";
        let e = parse(lrc, SubtitleFormat::Lrc).unwrap();
        assert_eq!(e[0].text, "LRC 文本");
        assert_eq!(e[0].start, 7.0);

        let ass = concat!(
            "[Events]\n",
            "Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n",
            "Dialogue: 0,0:00:08.00,0:00:09.00,Default,,0,0,0,,ASS 文本\n",
        );
        let f = parse(ass, SubtitleFormat::Ass).unwrap();
        assert_eq!(f[0].text, "ASS 文本");
        assert_eq!(f[0].start, 8.0);

        let ttml = concat!(
            r#"<tt xmlns="http://www.w3.org/ns/ttml"><body><div>"#,
            r#"<p begin="00:00:10.000" end="00:00:11.000">TTML 文本</p>"#,
            "</div></body></tt>",
        );
        let g = parse(ttml, SubtitleFormat::Ttml).unwrap();
        assert_eq!(g[0].text, "TTML 文本");
        assert_eq!(g[0].start, 10.0);

        // LRC 唱词里没有 `[Events]` / `Dialogue:`，交给 ASS 解析应当报错而不是硬套。
        assert!(parse(lrc, SubtitleFormat::Ass).is_err());
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

    /// UTF-16LE（记事本「Unicode」另存，BOM `FF FE`）：不能因为 UTF-8 解码失败就掉进
    /// GBK 分支报误导错误，必须先认 BOM 再按小端解回中文。
    #[test]
    fn read_subtitle_file_decodes_utf16le_bom() {
        let dir = temp_dir("utf16le");
        let path = dir.join("utf16le.srt");
        let content = "1\n00:00:01,000 --> 00:00:02,000\n你好世界\n";
        let mut bytes = vec![0xFF, 0xFE];
        for unit in content.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        std::fs::write(&path, &bytes).unwrap();

        let segs = read_subtitle_file(&path).unwrap();
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].text, "你好世界");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// UTF-16BE（BOM `FE FF`）：大端序同样要先认 BOM 再解，字节序不能搞反。
    #[test]
    fn read_subtitle_file_decodes_utf16be_bom() {
        let dir = temp_dir("utf16be");
        let path = dir.join("utf16be.srt");
        let content = "1\n00:00:01,000 --> 00:00:02,000\n你好世界\n";
        let mut bytes = vec![0xFE, 0xFF];
        for unit in content.encode_utf16() {
            bytes.extend_from_slice(&unit.to_be_bytes());
        }
        std::fs::write(&path, &bytes).unwrap();

        let segs = read_subtitle_file(&path).unwrap();
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].text, "你好世界");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 扩展名不认识时要在读盘前就报错，并带上路径方便用户定位问题文件。
    #[test]
    fn read_subtitle_file_rejects_unknown_extension() {
        let dir = temp_dir("badext");
        let path = dir.join("movie.xml");
        std::fs::write(&path, "whatever").unwrap();

        let err = read_subtitle_file(&path).unwrap_err().to_string();
        assert!(err.contains("movie.xml"), "错误信息应带路径: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 用真实导出器做一次「导出 → 导入」往返：结构化字段必须逐个原样回来。
    /// 这条钉住导入与导出的字段名 / 语义对齐，任何一侧改名都会在这里断。
    #[test]
    fn json_export_import_round_trip_preserves_fields() {
        use crate::subtitle::json::JsonSubtitleExporter;
        use crate::subtitle::segment::WordTiming;
        use crate::subtitle::ExportMode;
        let dir = temp_dir("json_rt");
        let path = dir.join("rt.json");

        let mut first = Segment::new(1, 0.0, 1.5, "你好");
        first.translation = Some("Hello".to_string());
        first.translation_lang = Some("English".to_string());
        first.language = Some("zh".to_string());
        first.confidence = Some(-0.42);
        first.speaker = Some(3);
        first.words = vec![
            WordTiming {
                word: "你".to_string(),
                start: 0.0,
                end: 0.7,
            },
            WordTiming {
                word: "好".to_string(),
                start: 0.7,
                end: 1.5,
            },
        ];
        let source = vec![first, Segment::new(2, 1.5, 3.0, "第二句")];

        JsonSubtitleExporter::write_to_file(
            &source,
            &path,
            Some("demo.mp4"),
            ExportMode::Bilingual,
            Some("English"),
        )
        .unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let back = parse_json(&raw).unwrap();

        assert_eq!(back.len(), 2, "往返后条数必须一致");
        let seg = &back[0];
        assert_eq!(seg.index, 1);
        assert_eq!(seg.start, 0.0);
        assert_eq!(seg.end, 1.5);
        // 导出的 `text` 是双语渲染结果，`raw_text` 才是原文，导入必须落在后者上
        assert_eq!(seg.text, "你好", "正文应取导出的 raw_text（原文）");
        assert_eq!(seg.polished, "", "polished 不是导出字段，往返后保持空");
        assert_eq!(seg.display_text(), "你好", "display_text 必须与导出前一致");
        assert_eq!(seg.translation.as_deref(), Some("Hello"));
        assert_eq!(seg.translation_lang.as_deref(), Some("English"));
        assert_eq!(seg.language.as_deref(), Some("zh"));
        assert_eq!(seg.confidence, Some(-0.42));
        assert_eq!(seg.speaker, Some(3));
        assert_eq!(seg.words, source[0].words, "词级时间戳往返必须逐条一致");

        let clean = &back[1];
        assert_eq!(clean.index, 2);
        assert_eq!(clean.text, "第二句");
        assert_eq!(clean.translation, None, "没有译文的块往返后仍是 None");
        assert!(clean.words.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
    /// 手工 / 第三方文档里 `text` 常被塞进双语拼接或润色后的整段，`raw_text` 才是
    /// 干净原文；导入必须优先取 `raw_text`，否则原文会被译文污染。
    #[test]
    fn json_prefers_raw_text_over_text() {
        let doc = r#"[{"start":0,"end":1,"text":"你好\nHello","raw_text":"你好"}]"#;
        let segs = parse_json(doc).unwrap();
        assert_eq!(segs[0].text, "你好", "正文应优先取 raw_text");

        // raw_text 缺失时回退到 text：少一个字段不该让整块消失
        let doc2 = r#"[{"start":0,"end":1,"text":"只有 text"}]"#;
        assert_eq!(parse_json(doc2).unwrap()[0].text, "只有 text");

        // 纯空白 raw_text 视同缺失，同样回退
        let doc3 = r#"[{"start":0,"end":1,"text":"回退目标","raw_text":"   "}]"#;
        assert_eq!(parse_json(doc3).unwrap()[0].text, "回退目标");
    }

    /// 只有时间轴 + 正文的最小文档必须能导入：其余字段一律 None / 空表，
    /// 「缺元数据」不是坏块。
    #[test]
    fn json_missing_optional_fields_become_none_and_empty_words() {
        let segs = parse_json(r#"[{"start":1.5,"end":2.5,"text":"最小块"}]"#).unwrap();
        let seg = &segs[0];
        assert_eq!(seg.index, 1);
        assert_eq!(seg.start, 1.5);
        assert_eq!(seg.end, 2.5);
        assert_eq!(seg.text, "最小块");
        assert_eq!(seg.translation, None, "缺失 translation 应为 None");
        assert_eq!(seg.translation_lang, None);
        assert_eq!(seg.language, None);
        assert_eq!(seg.confidence, None);
        assert_eq!(seg.speaker, None);
        assert!(seg.words.is_empty(), "缺失 words 应为空表");
        assert_eq!(seg.polished, "");

        // 显式 null 与纯空白字符串同样按「没有」处理：空白译文若存成 Some("")，
        // 下游会把该段判成「已翻译」，质检显示 100% 而画面上什么都没有。
        let blank = r#"[{"start":0,"end":1,"text":"x","translation":"   ","translation_lang":" ",
        "language":null,"confidence":null,"speaker":null,"words":null}]"#;
        let seg2 = &parse_json(blank).unwrap()[0];
        assert_eq!(seg2.translation, None, "纯空白译文按 None 处理");
        assert_eq!(seg2.translation_lang, None);
        assert_eq!(seg2.language, None);
        assert_eq!(seg2.confidence, None);
        assert_eq!(seg2.speaker, None);
        assert!(seg2.words.is_empty());
        assert!(!seg2.has_translation(), "空白译文不算已翻译");
    }
    /// `words` 形状不符（不是数组 / 词条缺字段）时只丢词级信息，整块字幕必须保留：
    /// 词级时间戳是可选增强信息，不该成为丢掉一条字幕的理由。
    #[test]
    fn json_words_shape_mismatch_becomes_empty_without_dropping_block() {
        let doc = r#"[{"start":0,"end":1,"text":"保留我","words":"不是数组"}]"#;
        let segs = parse_json(doc).unwrap();
        assert_eq!(segs.len(), 1, "words 形状不符不该丢整块");
        assert_eq!(segs[0].text, "保留我");
        assert!(segs[0].words.is_empty(), "形状不符时词级时间戳置空");

        let doc2 = r#"[{"start":0,"end":1,"text":"也保留","words":[{"word":"a","start":0}]}]"#;
        let segs2 = parse_json(doc2).unwrap();
        assert_eq!(segs2.len(), 1, "词条缺字段同样只丢词级信息");
        assert!(segs2[0].words.is_empty());
    }

    /// 坏块（缺 end / end 早于 start / 元素不是对象）只丢自己，好块照常导入，
    /// 且输出序号按出现顺序重排 1..n——输入序号错乱不能带进结果。
    #[test]
    fn json_skips_bad_blocks_and_renumbers_indices() {
        let doc = r#"{"segments":[
        {"index":7,"start":0,"end":1,"text":"第一"},
        {"start":1,"text":"缺 end"},
        {"start":2,"end":1.5,"text":"时间倒挂"},
        42,
        {"index":7,"start":3,"end":4,"text":"第二"}
    ]}"#;
        let segs = parse_json(doc).unwrap();
        assert_eq!(segs.len(), 2, "只保留两个好块");
        assert_eq!(segs[0].text, "第一");
        assert_eq!(segs[1].text, "第二");
        assert_eq!(segs[0].index, 1);
        assert_eq!(segs[1].index, 2, "序号必须重排为 1..n，不复用输入序号");
    }
    /// 非法 JSON、根对象缺 segments、segments 不是数组、空数组四种情况都必须报错，
    /// 且错误文案要能定位问题（提到 JSON / segments），不能是一句无信息的失败。
    #[test]
    fn json_errors_when_document_is_not_json_or_has_no_segments() {
        let not_json = parse_json("这不是 JSON").unwrap_err().to_string();
        assert!(
            not_json.contains("JSON"),
            "非法 JSON 的错误要说明 JSON: {not_json}"
        );

        let no_segments = parse_json(r#"{"format":"x"}"#).unwrap_err().to_string();
        assert!(
            no_segments.contains("segments"),
            "缺 segments 数组要指明字段名: {no_segments}"
        );

        let segments_not_array = parse_json(r#"{"segments":{}}"#).unwrap_err().to_string();
        assert!(
            segments_not_array.contains("segments"),
            "segments 不是数组也要报结构错误: {segments_not_array}"
        );

        let empty = parse_json("[]").unwrap_err().to_string();
        assert!(
            empty.contains("JSON"),
            "空数组应报「没有可用字幕块」: {empty}"
        );
    }

    /// 裸数组（手写 / 第三方工具的简化文档）必须与完整文档同样接受。
    #[test]
    fn json_accepts_bare_segments_array() {
        let doc = r#"[{"start":0,"end":1,"text":"甲"},{"start":1,"end":2,"text":"乙"}]"#;
        let segs = parse_json(doc).unwrap();
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].text, "甲");
        assert_eq!(segs[1].text, "乙");
        assert_eq!(segs[0].index, 1);
        assert_eq!(segs[1].index, 2);
    }

    /// 带 UTF-8 BOM 的 JSON（记事本另存）必须先剥 BOM 再解析，
    /// 否则第一块会连 BOM 一起被当成非法 JSON，整份文件都读不进来。
    #[test]
    fn json_strips_bom_before_parsing() {
        let doc = format!("\u{FEFF}{}", r#"[{"start":0,"end":1,"text":"带 BOM"}]"#);
        let segs = parse_json(&doc).unwrap();
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].text, "带 BOM");
    }

    /// 时间轴写成字符串（手写 / 第三方工具常见）时同样要能导入：JSON 数字与
    /// `"MM:SS.mmm"` 字符串走同一条解析路径，否则同一份文档换个写法就整篇读不进来。
    #[test]
    fn json_accepts_string_timestamps_via_parse_timestamp() {
        let doc = r#"[{"start":"00:00:01.500","end":"00:00:02,250","text":"字符串时间轴"}]"#;
        let segs = parse_json(doc).unwrap();
        assert_eq!(segs[0].start, 1.5, "字符串起始时间必须按秒解析");
        assert_eq!(segs[0].end, 2.25, "逗号与点号作毫秒分隔符都要认");
        assert_eq!(segs[0].text, "字符串时间轴");
    }

    /// LRC 只给起始时间，一条的结束时间必须取**下一条时间标签的起点**：
    /// 取错会让整篇时间轴整体错位，这是导入 LRC 最基础的形状。
    #[test]
    fn lrc_reads_basic_lines_with_end_from_next_start() {
        let content = "[00:01.00]第一句\n[00:05.50]第二句\n";
        let segs = parse_lrc(content).unwrap();
        assert_eq!(segs.len(), 2, "两行唱词应产出两条字幕");
        assert_eq!(segs[0].index, 1, "输出序号应从 1 重排");
        assert_eq!(segs[0].start, 1.0, "起始时间应取标签里的秒数");
        assert_eq!(segs[0].end, 5.5, "结束时间应取下一标签的起点");
        assert_eq!(segs[0].text, "第一句", "正文应保真");
        assert_eq!(segs[1].index, 2, "第二条序号为 2");
        assert_eq!(segs[1].start, 5.5, "第二条起点应接第一条终点");
        assert_eq!(segs[1].text, "第二句", "第二条正文应保真");
    }

    /// 一行多个时间标签（副歌重复出现）时只取第一个能解析成时间的标签：
    /// 取错会让这一句跑到别的时间点，其余标签只当分隔符。
    #[test]
    fn lrc_uses_first_tag_when_line_has_multiple_timestamps() {
        let content = "[00:10.00][00:20.00]副歌\n";
        let segs = parse_lrc(content).unwrap();
        assert_eq!(segs.len(), 1, "一行多标签仍只是一条唱词");
        assert_eq!(segs[0].start, 10.0, "应取第一个能解析成时间的标签");
        assert_eq!(segs[0].text, "副歌", "标签后的剩余文本才是正文");
    }

    /// 元数据标签（ti/ar/al/by/offset）解析不出时间，应被自然忽略；行首没有标签、
    /// 或 `[` 没有配对 `]` 的行整行跳过。三类混在一起测，确保不会互相连累。
    #[test]
    fn lrc_skips_metadata_tags_and_plain_lines() {
        let content = "\
[ti:标题]
[ar:歌手]
[al:专辑]
[by:制作者]
[offset:500]
这一行没有标签
[00:01.00]正文
[00:07.00缺少右括号
";
        let segs = parse_lrc(content).unwrap();
        assert_eq!(segs.len(), 1, "只有带时间标签的正文行才产出字幕");
        assert_eq!(segs[0].text, "正文", "正文应保真");
        assert_eq!(segs[0].start, 1.0, "起始时间应取标签里的秒数");
    }

    /// 最后一条没有「下一条」可依，只能按字数估算：短句落到最短时长下限，长句按
    /// 每字 0.3 秒 计算。这是估算值，必须可预测，用户才知道要不要校对。
    #[test]
    fn lrc_last_line_end_is_estimated_from_char_count() {
        let short = parse_lrc("[00:10.00]短\n").unwrap();
        assert!(
            (short[0].end - 10.0 - 1.5).abs() < 1e-9,
            "单字尾句应落到 1.5 秒下限: {}",
            short[0].end
        );

        let body = "十个字的一句话呀"; // 8 个字符
        let long = parse_lrc(&format!("[00:00.00]{body}\n")).unwrap();
        assert!(
            (long[0].end - 8.0 * 0.3).abs() < 1e-9,
            "长尾句应按每字 0.3 秒估算: {}",
            long[0].end
        );
    }

    /// `[00:10.00]` 后面没有字的时间行是「静音边界」：它本身不产出字幕，但仍是上一条
    /// 结束时间的候选——直接丢掉会让上一句一直唱到下一句开头。
    #[test]
    fn lrc_empty_text_tag_acts_as_silence_boundary() {
        let content = "[00:01.00]第一句\n[00:03.00]\n[00:08.00]第二句\n";
        let segs = parse_lrc(content).unwrap();
        assert_eq!(segs.len(), 2, "空文本的时间行本身不产出字幕");
        assert_eq!(segs[0].text, "第一句", "第一条正文应保真");
        assert_eq!(segs[0].end, 3.0, "空文本行仍是上一条结束时间的候选");
        assert_eq!(segs[1].text, "第二句", "空文本行之后的下一条照常解析");
        assert_eq!(segs[1].start, 8.0, "下一条起点应取自己的标签时间");
    }

    /// 时间倒挂只丢那一条：手改过的 LRC 常有一两处乱序，为它丢掉整篇等于让用户重做
    /// 一遍，因此其余唱词照常产出、序号照常重排。
    #[test]
    fn lrc_skips_out_of_order_blocks_but_keeps_good() {
        let content = "[00:20.00]倒挂\n[00:10.00]正常\n[00:30.00]最后\n";
        let segs = parse_lrc(content).unwrap();
        assert_eq!(segs.len(), 2, "倒挂的那一条应被丢掉");
        assert_eq!(segs[0].index, 1, "跳过的坏块不占序号");
        assert_eq!(segs[0].text, "正常", "倒挂之后的好块应保留");
        assert_eq!(segs[0].start, 10.0, "保留块起点应取自己的标签时间");
        assert_eq!(segs[0].end, 30.0, "保留块终点应取下一条起点");
        assert_eq!(segs[1].text, "最后", "最后一条应保留");
    }

    /// 一条可用唱词都没有时要报错而不是返回空表：返回空表会让调用方以为「导入成功但
    /// 字幕是空的」，用户拿不到任何反馈。
    #[test]
    fn lrc_errors_when_nothing_parsable() {
        assert!(parse_lrc("").is_err(), "空输入应报错");
        assert!(
            parse_lrc("[ti:标题]\n[ar:歌手]\n[offset:500]\n").is_err(),
            "只有元数据标签时应报错"
        );
        assert!(
            parse_lrc("这一行没有时间标签\n随便写写\n").is_err(),
            "没有时间标签时应报错"
        );
    }

    /// BOM 与 CRLF 是 Windows 记事本另存的默认产物：不处理的话第一行会变成「带 BOM 的
    /// 怪文本」而丢掉，时间戳行尾挂着 `\r` 也会解析失败。
    #[test]
    fn lrc_handles_bom_and_crlf() {
        let content = "\u{FEFF}[00:01.00]第一句\r\n[00:03.00]第二句\r\n";
        let segs = parse_lrc(content).unwrap();
        assert_eq!(segs.len(), 2, "BOM 与 CRLF 不应影响解析");
        assert_eq!(segs[0].text, "第一句", "正文应保真");
        assert_eq!(segs[0].end, 3.0, "结束时间仍应取下一标签起点");
    }

    /// 只解析 `[Events]` 段：别的段落里出现的 `Dialogue:` 行（注释模板、示例）一律不算
    /// 字幕，否则会凭空多出内容；`Comment:` 也不是对白。
    #[test]
    fn ass_reads_dialogue_lines_only_from_events_section() {
        let content = "\
[Script Info]
Title: 测试
Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,不该被读到

[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Dialogue: 0,0:00:03.00,0:00:04.00,Default,,0,0,0,,事件段里的对白
Comment: 0,0:00:04.00,0:00:05.00,Default,,0,0,0,,注释不是对白

[Fonts]
Dialogue: 0,0:00:06.00,0:00:07.00,Default,,0,0,0,,字体段里的伪对白
";
        let segs = parse_ass(content).unwrap();
        assert_eq!(segs.len(), 1, "只有 [Events] 段里的对白算字幕");
        assert_eq!(segs[0].index, 1, "序号应从 1 重排");
        assert_eq!(segs[0].start, 3.0, "起始时间应取 Start 字段");
        assert_eq!(segs[0].end, 4.0, "结束时间应取 End 字段");
        assert_eq!(segs[0].text, "事件段里的对白", "正文应保真");
    }

    /// 覆盖码 `{...}` 要整段丢掉，`\N` 与 `\n` 都要还原成换行（导出侧正是用 `\N` 表达
    /// 换行），`\h` 还原成空格；未闭合的 `{` 之后整段丢弃，避免半截指令混进字幕。
    #[test]
    fn ass_converts_newline_markers_and_strips_override_tags() {
        let content = "\
[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,{\\pos(10,10)}第一行\\N第二行\\n第三行\\h空格
Dialogue: 0,0:00:03.00,0:00:04.00,Default,,0,0,0,,保留{未闭合的覆盖码
";
        let segs = parse_ass(content).unwrap();
        assert_eq!(segs.len(), 2, "两条对白都应保留");
        assert_eq!(
            segs[0].text, "第一行\n第二行\n第三行 空格",
            "\\N 与 \\n 应还原成换行，\\h 应还原成空格"
        );
        assert_eq!(segs[1].text, "保留", "未闭合的覆盖码之后整段丢弃");
    }

    /// ASS 的 Name 字段是演员/说话人名：按首次出现顺序编号，同一人复用同一编号。
    /// 顺序错乱会让说话人归属整体错位（下游按编号分色、分组）。
    #[test]
    fn ass_maps_name_field_to_speaker_in_first_seen_order() {
        let content = "\
[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Dialogue: 0,0:00:01.00,0:00:02.00,Default,小明,0,0,0,,第一句
Dialogue: 0,0:00:02.00,0:00:03.00,Default,小红,0,0,0,,第二句
Dialogue: 0,0:00:03.00,0:00:04.00,Default,小明,0,0,0,,第三句
";
        let segs = parse_ass(content).unwrap();
        assert_eq!(segs.len(), 3, "三条对白都应保留");
        assert_eq!(segs[0].speaker, Some(0), "首个说话人编号为 0");
        assert_eq!(segs[1].speaker, Some(1), "第二个说话人编号为 1");
        assert_eq!(segs[2].speaker, Some(0), "同一说话人复用原编号");
    }

    /// Name 为空或 `Default` 时不应强加说话人标签：大量文件全篇写 `Default`，强行编号
    /// 只会给每句挂上无意义的「说话人 1」。大小写不同也要认。
    #[test]
    fn ass_default_name_leaves_speaker_empty() {
        let content = "\
[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Dialogue: 0,0:00:01.00,0:00:02.00,Default,Default,0,0,0,,第一句
Dialogue: 0,0:00:02.00,0:00:03.00,Default,,0,0,0,,第二句
Dialogue: 0,0:00:03.00,0:00:04.00,Default,DEFAULT,0,0,0,,第三句
";
        let segs = parse_ass(content).unwrap();
        assert_eq!(segs.len(), 3, "三条对白都应保留");
        for seg in &segs {
            assert_eq!(seg.speaker, None, "Default 或空 Name 不应产生说话人");
        }
    }

    /// 段内只认 `Dialogue:`：`Comment:` 要跳过；时间戳解析失败、`end < start`、字段数
    /// 不足 10 的坏行也各自跳过，但都不影响后面的好行。
    #[test]
    fn ass_skips_comment_and_bad_time_lines() {
        let content = "\
[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Comment: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,注释不是字幕
Dialogue: 0,坏时间,0:00:03.00,Default,,0,0,0,,坏开始时间
Dialogue: 0,0:00:05.00,0:00:04.00,Default,,0,0,0,,时间倒挂
Dialogue: 0,0:00:06.00,0:00:07.00,Default,,0,0,0
Dialogue: 0,0:00:08.00,0:00:09.00,Default,,0,0,0,,好行
";
        let segs = parse_ass(content).unwrap();
        assert_eq!(segs.len(), 1, "只有最后一行是可用对白");
        assert_eq!(segs[0].index, 1, "跳过的坏块不占序号");
        assert_eq!(segs[0].start, 8.0, "起始时间应取 Start 字段");
        assert_eq!(segs[0].end, 9.0, "结束时间应取 End 字段");
        assert_eq!(segs[0].text, "好行", "正文应保真");
    }

    /// Text 是最后一个字段，里面可以有逗号：必须用 `splitn(10, ...)` 让 limit 保住剩余
    /// 内容，否则文本会在第一个逗号处被截断，而中英对照里逗号极常见。
    #[test]
    fn ass_keeps_commas_inside_text_field() {
        let content = "\
[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,第一句,还有,两个逗号
";
        let segs = parse_ass(content).unwrap();
        assert_eq!(segs[0].text, "第一句,还有,两个逗号", "逗号必须保真");
    }

    /// 没有 `[Events]` 段、或段内一条 `Dialogue:` 都没有时要报错，理由同 LRC：
    /// 返回空表会让调用方以为导入成功，用户拿不到任何反馈。
    #[test]
    fn ass_errors_when_no_dialogue() {
        assert!(parse_ass("").is_err(), "空输入应报错");
        assert!(
            parse_ass("[Script Info]\nTitle: 测试\n").is_err(),
            "没有 [Events] 段时应报错"
        );
        let only_comment = concat!(
            "[Events]\n",
            "Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n",
            "Comment: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,只有注释\n",
        );
        assert!(parse_ass(only_comment).is_err(), "只有注释时应报错");
    }

    /// 同理，ASS 文件在 Windows 上几乎都是 CRLF，第三方工具还会加 BOM：两者都要先处理
    /// 掉，否则 `[Events]` 段名都认不出来，整篇被判成「没有对白」。
    #[test]
    fn ass_handles_bom_and_crlf() {
        let content = concat!(
            "\u{FEFF}[Events]\r\n",
            "Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\r\n",
            "Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,对白\r\n",
        );
        let segs = parse_ass(content).unwrap();
        assert_eq!(segs.len(), 1, "BOM 与 CRLF 不应影响解析");
        assert_eq!(segs[0].text, "对白", "正文应保真");
        assert_eq!(segs[0].start, 1.0, "起始时间应取 Start 字段");
    }

    /// 建一条测试用 `Segment`，顺带填上译文与译文语言。
    fn ttml_test_segment(
        start: f64,
        end: f64,
        text: &str,
        translation: Option<&str>,
        translation_lang: Option<&str>,
    ) -> Segment {
        let mut segment = Segment::new(0, start, end, text);
        segment.translation = translation.map(str::to_string);
        segment.translation_lang = translation_lang.map(str::to_string);
        segment
    }

    /// EBU-TT-D 单语文件的基线：`<p>` 的时间轴与正文都要读对，序号按出现顺序重排。
    #[test]
    fn ttml_reads_ebu_ttd_single_language() {
        let content = r#"<?xml version="1.0" encoding="UTF-8"?>
<tt xmlns="http://www.w3.org/ns/ttml" xml:lang="zh" ttp:timeBase="media" ttp:cellResolution="32 15">
  <body><div>
    <p xml:id="p1" begin="00:00:01.000" end="00:00:02.500">第一句</p>
    <p xml:id="p2" begin="00:00:03.000" end="00:00:04.000">第二句</p>
  </div></body>
</tt>
"#;
        let segs = parse_ttml(content).unwrap();
        assert_eq!(segs.len(), 2, "两句都应解析出来");
        assert_eq!(segs[0].text, "第一句");
        assert_eq!(segs[0].start, 1.0);
        assert_eq!(segs[0].end, 2.5);
        assert_eq!(segs[0].index, 1, "输出序号应重排为 1..n");
        assert_eq!(segs[1].text, "第二句");
        assert_eq!(segs[1].index, 2);
        assert_eq!(segs[1].start, 3.0);
    }

    /// 文档语言是切分基准：标了 `xml:lang="en"` 的 span 是译文，裸文本（文档语言）是
    /// 原文；相邻同语言的 span 必须合并成一句，不能被逐片拆成多条字幕。
    #[test]
    fn ttml_bilingual_split_by_document_language() {
        let content = r#"<tt xmlns="http://www.w3.org/ns/ttml" xml:lang="zh" ttp:timeBase="media">
  <body><div>
    <p begin="1s" end="2.5s"><span xml:lang="en">Hello</span><br/>你好</p>
    <p begin="3s" end="4s"><span>Hel</span><span>lo</span></p>
  </div></body>
</tt>
"#;
        let segs = parse_ttml(content).unwrap();
        assert_eq!(segs.len(), 2, "两句都应解析");
        assert_eq!(segs[0].text, "你好", "文档语言一侧应作为原文");
        assert_eq!(
            segs[0].translation.as_deref(),
            Some("Hello"),
            "另一种语言应作为译文"
        );
        assert_eq!(segs[0].translation_lang.as_deref(), Some("en"));
        assert_eq!(segs[1].text, "Hello", "相邻同语言 span 应合并成一句");
        assert!(segs[1].translation.is_none(), "单语段落不应凭空长出译文");
    }

    /// Netflix TTAL 常常不写根 `xml:lang`：此时以第一个带语言的 span 作为原文基准，
    /// 避免把「未声明文档语言」误判成「整篇都是译文」。
    #[test]
    fn ttml_netflix_ttal_without_doc_lang_uses_first_span_as_original() {
        let content = r#"<tt xmlns="http://www.w3.org/ns/ttml" ttp:timeBase="media" ttp:contentProfiles="http://www.netflix.com/ttml/profile/ntflx-ttal-1.0">
  <body><div>
    <p begin="00:00:01.000" end="00:00:02.000"><span xml:lang="en">Hello</span><br/>你好</p>
  </div></body>
</tt>
"#;
        let segs = parse_ttml(content).unwrap();
        assert_eq!(segs.len(), 1);
        assert_eq!(
            segs[0].text, "Hello",
            "无文档语言时以第一个 span 语言为原文"
        );
        assert!(segs[0].translation.is_none(), "没有第二种语言就没有译文");
    }

    /// `<br/>` 必须变成真正的换行（TTML 用换行而非空格断行）；预定义实体要还原、
    /// 未知实体（如 `&nbsp;`）要原样保留。
    #[test]
    fn ttml_br_becomes_newline_and_entities_are_decoded() {
        let content = r#"<tt xmlns="http://www.w3.org/ns/ttml" xml:lang="en">
  <body><div>
    <p begin="0s" end="1s">A &amp; B &lt;tag&gt; AB &nbsp;</p>
    <p begin="2s" end="3s">上<BR />下</p>
  </div></body>
</tt>
"#;
        let segs = parse_ttml(content).unwrap();
        assert_eq!(segs.len(), 2);
        assert_eq!(
            segs[0].text, "A & B <tag> AB &nbsp;",
            "转义应还原，未知实体原样保留"
        );
        assert_eq!(segs[1].text, "上\n下", "<br/> 应变成换行");
    }

    /// 说话人映射：`spk{N}`（N 从 1 起）还原成 0 起的编号，非 `spk` 前缀的 id 不映射。
    #[test]
    fn ttml_ttm_agent_maps_to_speaker() {
        let content = r#"<tt xmlns="http://www.w3.org/ns/ttml" xml:lang="zh">
  <body><div>
    <p begin="0s" end="1s" ttm:agent="spk1">甲</p>
    <p begin="1s" end="2s" ttm:agent="spk2">乙</p>
    <p begin="2s" end="3s" ttm:agent="narrator">旁白</p>
    <p begin="3s" end="4s">无标签</p>
  </div></body>
</tt>
"#;
        let segs = parse_ttml(content).unwrap();
        assert_eq!(segs.len(), 4);
        assert_eq!(segs[0].speaker, Some(0), "spk1 应映射成 0 号说话人");
        assert_eq!(segs[1].speaker, Some(1), "spk2 应映射成 1 号说话人");
        assert_eq!(segs[2].speaker, None, "非 spk 前缀的 id 不应映射");
        assert_eq!(segs[3].speaker, None, "无 agent 属性不应映射");
    }

    /// 坏块一律跳过、好块照常保留：缺时间轴、时间戳非法、end < begin、空正文、
    /// 自闭合 `<p/>` 各丢一块，剩下两块并重排序号。这是容错原则的核心回归。
    #[test]
    fn ttml_skips_bad_blocks_but_keeps_good() {
        let content = r#"<tt xmlns="http://www.w3.org/ns/ttml" xml:lang="zh">
  <body><div>
    <p xml:id="p1">缺时间轴</p>
    <p begin="not-a-time" end="2s">坏时间戳</p>
    <p begin="5s" end="3s">时间倒挂</p>
    <p begin="6s" end="7s"></p>
    <p begin="8s" end="9s" />
    <p begin="10s" end="11s">好的一</p>
    <p begin="12s" end="13s">好的二</p>
  </div></body>
</tt>
"#;
        let segs = parse_ttml(content).unwrap();
        assert_eq!(segs.len(), 2, "坏块应被跳过，只留两条好块");
        assert_eq!(segs[0].text, "好的一");
        assert_eq!(segs[1].text, "好的二");
        assert_eq!(segs[0].index, 1, "序号应重排为 1..n");
        assert_eq!(segs[1].index, 2);
    }

    /// TTML 的「数字 + 单位」偏移写法：`ms`/`h`/`m`/`s` 都要支持，缺 `end` 时用
    /// `begin + dur`；帧式时间（`10f`）需要帧率、无法可靠换算，按坏块跳过。
    #[test]
    fn ttml_offset_time_unit_suffix_is_supported() {
        let content = r#"<tt xmlns="http://www.w3.org/ns/ttml" xml:lang="zh" ttp:frameRate="25">
  <body><div>
    <p begin="1.5s" end="2s">A</p>
    <p begin="500ms" end="1.5s">B</p>
    <p begin="90m" end="2h">C</p>
    <p begin="1s" dur="2.5s">D</p>
    <p begin="10f" end="11f">帧时间不支持</p>
  </div></body>
</tt>
"#;
        let segs = parse_ttml(content).unwrap();
        assert_eq!(segs.len(), 4, "帧式时间应作为坏块跳过");
        assert_eq!(segs[0].start, 1.5);
        assert_eq!(segs[0].end, 2.0);
        assert_eq!(segs[1].start, 0.5, "500ms 应换算成 0.5 秒");
        assert_eq!(segs[1].end, 1.5);
        assert_eq!(segs[2].start, 5400.0, "90 分钟应换算成秒");
        assert_eq!(segs[2].end, 7200.0, "2 小时应换算成秒");
        assert_eq!(segs[3].start, 1.0);
        assert_eq!(segs[3].end, 3.5, "end 缺失时用 begin + dur");
    }

    /// 整篇没有任何可用块时必须报错（而不是返回空表），错误信息要指明是 TTML，
    /// 否则用户无从判断是哪一种字幕读失败了。
    #[test]
    fn ttml_errors_when_no_paragraphs() {
        let empty = parse_ttml("").unwrap_err().to_string();
        assert!(
            empty.contains("TTML"),
            "空文档的错误信息应指明 TTML: {empty}"
        );

        let no_p = r#"<tt xmlns="http://www.w3.org/ns/ttml" xml:lang="zh">
  <body><div></div></body>
</tt>
"#;
        assert!(parse_ttml(no_p).is_err(), "没有 <p> 应报错");

        let all_bad = r#"<tt xmlns="http://www.w3.org/ns/ttml" xml:lang="zh">
  <body><div>
    <p>无时间轴</p>
    <p begin="bad" end="worse">坏时间戳</p>
  </div></body>
</tt>
"#;
        assert!(parse_ttml(all_bad).is_err(), "全是坏块应报错");
    }

    /// 往返闭环：导出器写出的 EBU-TT-D / Netflix TTAL 再读回来，文本、译文、译文语言
    /// 与时间轴都必须一致。这条测试同时钉住「双语 span + `<br/>` + 裸原文」的互逆关系。
    #[test]
    fn ttml_export_import_round_trip_preserves_text() {
        use crate::subtitle::{ExportMode, TtmlExporter, TtmlProfile};

        let segments = vec![
            ttml_test_segment(1.0, 2.5, "第一句中文", Some("First line"), Some("en")),
            ttml_test_segment(
                3.0,
                4.25,
                "第二句 & 更长的中文",
                Some("Second line"),
                Some("en"),
            ),
        ];
        let dir = temp_dir("ttml_roundtrip");
        for (tag, profile) in [
            ("ebu", TtmlProfile::EbuTtD),
            ("ttal", TtmlProfile::NetflixTtal),
        ] {
            let path = dir.join(format!("out_{tag}.ttml"));
            TtmlExporter::write_to_file(
                &segments,
                &path,
                "zh",
                Some("en"),
                ExportMode::Bilingual,
                profile,
            )
            .unwrap();
            let written = std::fs::read_to_string(&path).unwrap();
            let read_back = parse_ttml(&written).unwrap();
            assert_eq!(read_back.len(), 2, "{tag}: 往返后应仍是两句");
            assert_eq!(read_back[0].text, "第一句中文", "{tag}: 原文应保真");
            assert_eq!(
                read_back[0].translation.as_deref(),
                Some("First line"),
                "{tag}: 译文应保真"
            );
            assert_eq!(
                read_back[0].translation_lang.as_deref(),
                Some("en"),
                "{tag}: 译文语言应保真"
            );
            assert_eq!(read_back[0].start, 1.0, "{tag}: 起始时间应保真");
            assert_eq!(read_back[0].end, 2.5, "{tag}: 结束时间应保真");
            assert_eq!(
                read_back[1].text, "第二句 & 更长的中文",
                "{tag}: 转义与还原应闭环"
            );
            assert_eq!(read_back[1].start, 3.0);
            assert_eq!(read_back[1].end, 4.25);
            let _ = std::fs::remove_file(&path);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// SBV：时间轴行 + 正文行交替，毫秒用点号、逗号分隔起止。
    #[test]
    fn sbv_reads_timing_and_text_pairs() {
        let content = "0:00:01.000,0:00:02.500\n第一句\n\n0:00:03.000,0:00:04.000\n第二句,含逗号\n";
        let segs = parse_sbv(content).unwrap();
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].index, 1, "序号应从 1 重排");
        assert_eq!(segs[0].start, 1.0);
        assert_eq!(segs[0].end, 2.5);
        assert_eq!(segs[0].text, "第一句");
        // 正文里的逗号不能让时间轴行被误判（`split_once` 取第一段即止）
        assert_eq!(segs[1].text, "第二句,含逗号");
        assert_eq!(segs[1].start, 3.0);
        assert_eq!(segs[1].end, 4.0);
    }

    /// SBV 的三种坏块各自只丢自己：时间轴连着时间轴、时间倒挂、正文为空。
    #[test]
    fn sbv_skips_bad_blocks_but_keeps_good_ones() {
        let content = concat!(
            "0:00:01.000,0:00:02.000\n",   // 时间轴
            "0:00:05.000,0:00:06.000\n",   // 又是时间轴：上一条没有正文，丢掉
            "这条正文\n",                  // 消费上一条时间轴
            "0:00:07.000,0:00:06.000\n",   // 倒挂：丢掉
            "0:00:08.000,0:00:09.000\n\n", // 正文为空：丢掉
            "0:00:10.000,0:00:11.000\n好行\n",
        );
        let segs = parse_sbv(content).unwrap();
        assert_eq!(segs.len(), 2, "只保留两条完整块: {:?}", segs);
        assert_eq!(segs[0].text, "这条正文");
        assert_eq!(segs[0].start, 5.0);
        assert_eq!(segs[1].text, "好行");
        assert_eq!(segs[1].index, 2, "跳过的坏块不占序号");
    }

    /// 文件头部的说明文字（没有对应时间轴的裸行）必须被忽略，而不是变成字幕。
    #[test]
    fn sbv_ignores_bare_lines_without_timing() {
        let content = "这是文件头说明\n0:00:01.000,0:00:02.000\n正文\n";
        let segs = parse_sbv(content).unwrap();
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].text, "正文");
    }

    /// 一条可用块都没有时要报错，而不是返回空表让调用方以为「导入成功但字幕是空的」。
    #[test]
    fn sbv_errors_when_nothing_parsable() {
        assert!(parse_sbv("").is_err(), "空输入应报错");
        assert!(parse_sbv("只有正文\n没有时间轴\n").is_err());
    }

    /// 扩展名识别：`.sbv`（大小写不敏感）必须被认出来。
    #[test]
    fn sbv_extension_is_recognized() {
        assert_eq!(
            SubtitleFormat::from_path(Path::new("a.sbv")),
            Some(SubtitleFormat::Sbv)
        );
        assert_eq!(
            SubtitleFormat::from_path(Path::new("b.SBV")),
            Some(SubtitleFormat::Sbv)
        );
        assert_eq!(SubtitleFormat::Sbv.label(), "SBV 字幕");
    }

    /// 导出侧写出的 SBV 必须能被导入侧原样读回（时间与文本都保真）。
    #[test]
    fn sbv_export_import_round_trip() {
        use crate::subtitle::writer::SubtitleWriter;

        let segments = vec![
            Segment::new(1, 1.0, 2.5, "第一句"),
            Segment::new(2, 65.25, 66.0, "第二句"),
        ];
        let dir = temp_dir("sbv_roundtrip");
        let path = dir.join("out.sbv");
        SubtitleWriter::write_to_file(&segments, &path, "sbv").unwrap();

        let read_back = parse_sbv(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(read_back.len(), 2, "往返后应仍是两句");
        for (want, got) in segments.iter().zip(&read_back) {
            assert_eq!(got.text, want.text, "文本应保真");
            assert!((got.start - want.start).abs() < 1e-9, "起点应保真");
            assert!((got.end - want.end).abs() < 1e-9, "终点应保真");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 导入要还原导出侧写出的实体：`&amp;`/`&lt;`/`&gt;` 逐个还原，且只还原**一遍**
    /// （`&amp;lt;` → `&lt;`），不认识的实体（`&nbsp;`）原样保留。
    #[test]
    fn markup_entities_are_decoded_once_on_import() {
        let segs = parse_srt("1\n00:00:01,000 --> 00:00:02,000\nA &amp; B &lt;x&gt; C\n").unwrap();
        assert_eq!(segs[0].text, "A & B <x> C");

        let segs = parse_srt("1\n00:00:01,000 --> 00:00:02,000\n&amp;lt;\n").unwrap();
        assert_eq!(segs[0].text, "&lt;", "只还原一遍，不得二次解码成 `<`");

        let segs = parse_srt("1\n00:00:01,000 --> 00:00:02,000\n&quot;&nbsp;x\n").unwrap();
        assert_eq!(segs[0].text, "&quot;&nbsp;x", "不认识的实体原样保留");
    }

    /// SRT / VTT / SBV 的「写 → 读 → 写」必须逐字节相等：含 `&`/`<`/`>` 的正文
    /// 每往返一次不该多叠一层实体，也不该少还原一层。
    #[test]
    fn markup_entities_round_trip_is_byte_stable() {
        use crate::subtitle::writer::SubtitleWriter;

        let dir = temp_dir("entity_rt");
        std::fs::create_dir_all(&dir).unwrap();
        let segs = vec![Segment::new(1, 1.0, 2.0, "a & b <i>x</i> > c &amp; d")];

        let first = dir.join("first.srt");
        let second = dir.join("second.srt");
        SubtitleWriter::write_srt(&segs, &first).unwrap();
        let back = parse_srt(&std::fs::read_to_string(&first).unwrap()).unwrap();
        assert_eq!(back[0].text, segs[0].text, "SRT 实体应还原回原文");
        SubtitleWriter::write_srt(&back, &second).unwrap();
        assert_eq!(
            std::fs::read_to_string(&first).unwrap(),
            std::fs::read_to_string(&second).unwrap(),
            "SRT 写→读→写 必须逐字节相等"
        );

        let first = dir.join("first.vtt");
        let second = dir.join("second.vtt");
        SubtitleWriter::write_to_file(&segs, &first, "vtt").unwrap();
        let back = parse_vtt(&std::fs::read_to_string(&first).unwrap()).unwrap();
        assert_eq!(back[0].text, segs[0].text, "VTT 实体应还原回原文");
        SubtitleWriter::write_to_file(&back, &second, "vtt").unwrap();
        assert_eq!(
            std::fs::read_to_string(&first).unwrap(),
            std::fs::read_to_string(&second).unwrap(),
            "VTT 写→读→写 必须逐字节相等"
        );

        let first = dir.join("first.sbv");
        let second = dir.join("second.sbv");
        SubtitleWriter::write_to_file(&segs, &first, "sbv").unwrap();
        let back = parse_sbv(&std::fs::read_to_string(&first).unwrap()).unwrap();
        assert_eq!(back[0].text, segs[0].text, "SBV 实体应还原回原文");
        SubtitleWriter::write_to_file(&back, &second, "sbv").unwrap();
        assert_eq!(
            std::fs::read_to_string(&first).unwrap(),
            std::fs::read_to_string(&second).unwrap(),
            "SBV 写→读→写 必须逐字节相等"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ASS 往返：导出侧把字面 `{`/`}` 写成 `\{`/`\}`，导入必须原样读回——
    /// 曾经 `\{` 里的 `{` 会被当成覆盖码起始，把两花括号之间的正文连同前缀一起吞掉。
    #[test]
    fn ass_export_import_round_trip_keeps_literal_braces() {
        use crate::subtitle::writer::SubtitleWriter;
        use crate::subtitle::ExportMode;
        use crate::utils::SubtitleStyleConfig;

        let dir = temp_dir("ass_braces_rt");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out.ass");
        let seg = Segment::new(1, 1.0, 2.0, "原文 {A} 结束");
        SubtitleWriter::write_ass_with_style(
            std::slice::from_ref(&seg),
            &path,
            ExportMode::RawOnly,
            &SubtitleStyleConfig::default(),
        )
        .unwrap();

        let back = parse_ass(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(
            back[0].text, "原文 {A} 结束",
            "字面花括号必须原样读回，不能被覆盖码逻辑吞掉"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// LRC 往返：导出只写起点、导入按下一行起点补终点；正文首尾空白两侧同口径 trim，
    /// 「写 → 读 → 写」必须逐字节相等。
    #[test]
    fn lrc_export_import_export_is_byte_stable() {
        use crate::subtitle::writer::SubtitleWriter;

        let dir = temp_dir("lrc_rt");
        std::fs::create_dir_all(&dir).unwrap();
        let segs = vec![
            Segment::new(1, 7.0, 9.0, "第一句"),
            Segment::new(2, 65.25, 66.0, "第二句"),
        ];
        let first = dir.join("first.lrc");
        let second = dir.join("second.lrc");
        SubtitleWriter::write_to_file(&segs, &first, "lrc").unwrap();
        let back = parse_lrc(&std::fs::read_to_string(&first).unwrap()).unwrap();
        assert_eq!(back.len(), 2, "往返后应仍是两句");
        assert_eq!(back[0].text, "第一句");
        assert!((back[1].start - 65.25).abs() < 1e-9, "起点应保真");
        SubtitleWriter::write_to_file(&back, &second, "lrc").unwrap();
        assert_eq!(
            std::fs::read_to_string(&first).unwrap(),
            std::fs::read_to_string(&second).unwrap(),
            "LRC 写→读→写 必须逐字节相等"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// VTT 的 cue 标识行可以叫 `NOTE1` / `STYLE2`：元数据关键字必须是独立词，
    /// 按前缀匹配会把这类 cue 整条当注释丢掉；真正的 NOTE/STYLE/REGION 仍要跳过。
    #[test]
    fn vtt_note_like_cue_ids_are_not_swallowed() {
        let content = "\
WEBVTT

NOTE1
00:00:01.000 --> 00:00:02.000
字幕一

STYLE2
00:00:03.000 --> 00:00:04.000
字幕二
";
        let segs = parse_vtt(content).unwrap();
        assert_eq!(
            segs.len(),
            2,
            "`NOTE1`/`STYLE2` 是 cue 标识行，不是元数据块"
        );
        assert_eq!(segs[0].text, "字幕一");
        assert_eq!(segs[1].text, "字幕二");

        let metadata = "\
WEBVTT

NOTE 注释
注释第二行

STYLE
::cue { color: white }

REGION
id:r1

00:00:01.000 --> 00:00:02.000
正文
";
        let segs = parse_vtt(metadata).unwrap();
        assert_eq!(segs.len(), 1, "真正的元数据块仍要整块跳过");
        assert_eq!(segs[0].text, "正文");
    }

    /// SBV 正文形如 `2023,2024` 时不能被当成时间轴：时间轴行两侧都必须含 `:`。
    #[test]
    fn sbv_text_that_looks_like_comma_times_is_kept_as_text() {
        let content = "\
0:00:01.000,0:00:02.000
2023,2024 年是关键

0:00:03.000,0:00:04.000
下一句
";
        let segs = parse_sbv(content).unwrap();
        assert_eq!(segs.len(), 2, "形如 `2023,2024` 的正文不能被当时间轴");
        assert_eq!(segs[0].text, "2023,2024 年是关键");
        assert!((segs[0].start - 1.0).abs() < 1e-9);
        assert!((segs[0].end - 2.0).abs() < 1e-9);
        assert_eq!(segs[1].text, "下一句");
    }

    /// TTML 注释里被注掉的 `<p>` 不该被解析成字幕（否则凭空多出几句）。
    #[test]
    fn ttml_ignores_commented_out_cues() {
        let content = "\
<tt xmlns=\"http://www.w3.org/ns/ttml\"><body><div>
<!-- <p begin=\"0s\" end=\"1s\">注掉的句子</p> -->
<p begin=\"1s\" end=\"2s\">真正的句子</p>
</div></body></tt>";
        let segs = parse_ttml(content).unwrap();
        assert_eq!(segs.len(), 1, "注释里的 <p> 不该变成字幕");
        assert_eq!(segs[0].text, "真正的句子");
    }

    /// TTML：CDATA 正文按字面取用，带命名空间前缀的 `<tt:p>` 也能读；
    /// `</p >`（名字后带空白）是合法 XML，不该让后续块全部丢失。
    #[test]
    fn ttml_reads_cdata_prefixed_elements_and_spaced_close_tag() {
        let cdata = "\
<tt:tt xmlns:tt=\"http://www.w3.org/ns/ttml\"><tt:body><tt:div>
<tt:p begin=\"0s\" end=\"1s\"><![CDATA[a < b & c]]></tt:p>
</tt:div></tt:body></tt:tt>";
        let segs = parse_ttml(cdata).unwrap();
        assert_eq!(segs.len(), 1, "带前缀的 <tt:p> 必须能读");
        assert_eq!(segs[0].text, "a < b & c", "CDATA 内容按字面保留");
        assert!((segs[0].end - 1.0).abs() < 1e-9);

        let spaced = "\
<tt><body><div>
<p begin=\"0s\" end=\"1s\">第一句</p >
<p begin=\"2s\" end=\"3s\">第二句</p>
</div></body></tt>";
        let segs = parse_ttml(spaced).unwrap();
        assert_eq!(segs.len(), 2, "`</p >` 不该让后续块全部丢失");
        assert_eq!(segs[0].text, "第一句");
        assert_eq!(segs[1].text, "第二句");
    }

    /// TTML 偏移量位数极多时会算出 inf：这种坏块必须丢弃，不能让 inf 时间进下游。
    #[test]
    fn ttml_rejects_non_finite_offsets() {
        let huge = "9".repeat(400);
        let content =
            format!("<tt><body><div><p begin=\"0s\" end=\"{huge}s\">哈</p></div></body></tt>");
        assert!(
            parse_ttml(&content).is_err(),
            "溢出成 inf 的偏移量必须按坏块丢弃"
        );
    }

    /// JSON 里的负时间戳会让下游算出负时长：夹到 0，而不是原样保留。
    #[test]
    fn json_negative_start_is_clamped() {
        let doc = r#"[{"start":-5,"end":1,"text":"负时间"}]"#;
        let segs = parse_json(doc).unwrap();
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].start, 0.0, "负起点必须夹到 0");
        assert_eq!(segs[0].text, "负时间");
    }

    /// 编码兜底：既不是干净 UTF-8 也不是干净 GBK 的文件用 lossy 兜底，
    /// 个别坏字节不该让整份字幕读失败，其余内容要保住。
    #[test]
    fn read_subtitle_file_lossy_fallback_keeps_good_bytes() {
        let dir = temp_dir("lossy");
        let path = dir.join("lossy.srt");
        let mut bytes = b"1\n00:00:01,000 --> 00:00:02,000\n".to_vec();
        bytes.extend_from_slice("你好".as_bytes());
        bytes.push(0xFF);
        bytes.push(b'\n');
        std::fs::write(&path, &bytes).unwrap();

        let segs = read_subtitle_file(&path).expect("坏字节不该让整份文件读失败");
        assert_eq!(segs.len(), 1);
        assert!(
            segs[0].text.starts_with("你好"),
            "坏字节前后的好内容必须保住: {:?}",
            segs[0].text
        );
        assert!(segs[0].text.contains('\u{FFFD}'), "坏字节应变成替换字符");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
