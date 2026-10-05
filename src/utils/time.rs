//! 时间戳转换与格式化工具

/// 将浮点秒转换为 SRT 时间格式: 00:01:23,456
pub fn seconds_to_srt_time(seconds: f64) -> String {
    let total_millis = (seconds.max(0.0) * 1000.0).round() as u64;
    let hours = total_millis / 3_600_000;
    let minutes = (total_millis % 3_600_000) / 60_000;
    let secs = (total_millis % 60_000) / 1000;
    let millis = total_millis % 1000;
    format!("{:02}:{:02}:{:02},{:03}", hours, minutes, secs, millis)
}

/// 将浮点秒转换为标准毫秒时间戳格式: 00:01:23.456
pub fn seconds_to_timestamp(seconds: f64) -> String {
    let total_millis = (seconds.max(0.0) * 1000.0).round() as u64;
    let hours = total_millis / 3_600_000;
    let minutes = (total_millis % 3_600_000) / 60_000;
    let secs = (total_millis % 60_000) / 1000;
    let millis = total_millis % 1000;
    format!("{:02}:{:02}:{:02}.{:03}", hours, minutes, secs, millis)
}

/// 将浮点秒转换为 ASS 时间格式: 0:01:23.45 (两位小数百分秒)
pub fn seconds_to_ass_time(seconds: f64) -> String {
    let total_centis = (seconds.max(0.0) * 100.0).round() as u64;
    let hours = total_centis / 360_000;
    let minutes = (total_centis % 360_000) / 6000;
    let secs = (total_centis % 6000) / 100;
    let centis = total_centis % 100;
    format!("{}:{:02}:{:02}.{:02}", hours, minutes, secs, centis)
}

/// 界面展示用的紧凑时间戳: 00:01:23.4（精确到 0.1 秒）
///
/// 只用于字幕清单 / 快速编辑卡这类**屏幕展示**，导出仍走 [`seconds_to_timestamp`]
/// 的毫秒精度——导出是数据，界面是观感，两者精度需求不同：毫秒后缀（`.456`）
/// 扫读时几乎无意义，却把时间列撑宽，挤掉原文与译文的空间。
pub fn seconds_to_timestamp_short(seconds: f64) -> String {
    let total_tenths = (seconds.max(0.0) * 10.0).round() as u64;
    let hours = total_tenths / 36_000;
    let minutes = (total_tenths % 36_000) / 600;
    let secs = (total_tenths % 600) / 10;
    let tenths = total_tenths % 10;
    format!("{:02}:{:02}:{:02}.{}", hours, minutes, secs, tenths)
}

/// 简短显示时间: 01:23
pub fn format_duration_short(seconds: f64) -> String {
    let total_secs = seconds.max(0.0).round() as u64;
    let mins = total_secs / 60;
    let secs = total_secs % 60;
    format!("{:02}:{:02}", mins, secs)
}

/// 将浮点秒转换为时分秒格式 (无毫秒): 00:01:23
pub fn seconds_to_hms(seconds: f64) -> String {
    let total_secs = seconds.max(0.0).round() as u64;
    let hours = total_secs / 3600;
    let mins = (total_secs % 3600) / 60;
    let secs = total_secs % 60;
    format!("{:02}:{:02}:{:02}", hours, mins, secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hms_time() {
        assert_eq!(seconds_to_hms(0.0), "00:00:00");
        assert_eq!(seconds_to_hms(65.123), "00:01:05");
        assert_eq!(seconds_to_hms(3661.0), "01:01:01");
    }

    #[test]
    fn test_srt_time() {
        assert_eq!(seconds_to_srt_time(0.0), "00:00:00,000");
        assert_eq!(seconds_to_srt_time(65.123), "00:01:05,123");
        assert_eq!(seconds_to_srt_time(3661.005), "01:01:01,005");
    }

    #[test]
    fn test_short_timestamp_keeps_one_decimal() {
        assert_eq!(seconds_to_timestamp_short(0.0), "00:00:00.0");
        // 0.46s 四舍五入到 0.5（不是截断成 0.4）
        assert_eq!(seconds_to_timestamp_short(0.46), "00:00:00.5");
        assert_eq!(seconds_to_timestamp_short(65.123), "00:01:05.1");
        assert_eq!(seconds_to_timestamp_short(3661.96), "01:01:02.0");
        // 负值归零，不产生 "-1" 这类怪异时基
        assert_eq!(seconds_to_timestamp_short(-3.0), "00:00:00.0");
    }

    #[test]
    fn test_ass_time() {
        assert_eq!(seconds_to_ass_time(0.0), "0:00:00.00");
        assert_eq!(seconds_to_ass_time(65.126), "0:01:05.13");
    }
}
