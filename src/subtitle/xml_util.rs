//! XML 转义与文本清洗（FCPXML / Premiere XML / TTML 家族共用）
//!
//! 为什么必须**共用一份**而不是各导出器各写一个 `escape_xml`：转义规则一旦分叉，
//! 就会出现「FCPXML 转义了 `&`、TTML 忘了转义 `<`」这类只在特定素材上炸的问题
//! （字幕文本里出现 `&`、`<` 很常见：歌词、数学、代码讲解、弹幕）。集中一处后，
//! 任何新增 XML 导出器只要调它，就自动获得同一套正确行为。
//!
//! 除了五个 XML 保留字符，这里还做一件容易被忽略但很关键的事：**剔除 XML 1.0
//! 不允许出现的控制字符**（除 `\t` `\n` `\r` 之外的 U+0000–U+001F，以及
//! U+FFFE/U+FFFF）。ASR 引擎或剪贴板粘贴偶尔会带进这些字节，它们会让
//! `&#x1;` 这种非法实体直接导致整个文件**无法被解析**——导出看起来成功，
//! 到了剪辑软件里才报「文件损坏」。宁可丢一个不可见字符，也不能让整份工程打不开。

/// 转义 XML 文本节点内容（`&` `<` `>`），并剔除非法控制字符。
pub fn escape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            // 合法空白：原样保留（TTML 的换行语义依赖它）
            '\t' | '\n' | '\r' => out.push(c),
            // XML 1.0 禁止的控制字符：丢弃而不是转义（转义也是非法的）
            c if (c as u32) < 0x20 || c == '\u{FFFE}' || c == '\u{FFFF}' => {}
            c => out.push(c),
        }
    }
    out
}

/// 转义 XML 属性值（在文本规则之上再加 `"` 与 `'`）。
pub fn escape_attr(s: &str) -> String {
    escape_text(s)
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_five_entities_in_the_right_places() {
        assert_eq!(escape_text("a & b < c > d"), "a &amp; b &lt; c &gt; d");
        assert_eq!(
            escape_attr("say \"hi\" & 'bye'"),
            "say &quot;hi&quot; &amp; &apos;bye&apos;"
        );
        // 文本节点里引号无需转义（转义了也不算错，但不必要）
        assert_eq!(escape_text("\"q\""), "\"q\"");
    }

    /// 控制字符必须被剔除：它们既不能原样写出，也不能写成实体。
    #[test]
    fn strips_illegal_control_chars() {
        let dirty = "前\u{0}后\u{1}尾\u{7}";
        assert_eq!(escape_text(dirty), "前后尾");
        // 合法空白保留
        assert_eq!(escape_text("a\tb\nc"), "a\tb\nc");
    }

    /// 中文、emoji、组合字符必须原样保留（TTML 是 UTF-8，不是 8 位字符集）。
    #[test]
    fn keeps_unicode_intact() {
        let s = "你好，世界 — «引用» 🎬";
        assert_eq!(escape_text(s), s);
    }
}
