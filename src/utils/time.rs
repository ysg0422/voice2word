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

/// 将浮点秒转换为 ASS 时间格式: 0:01:23.45 (两位小数百分秒)
pub fn seconds_to_ass_time(seconds: f64) -> String {
    let total_centis = (seconds.max(0.0) * 100.0).round() as u64;
    let hours = total_centis / 360_000;
    let minutes = (total_centis % 360_000) / 6000;
    let secs = (total_centis % 6000) / 100;
    let centis = total_centis % 100;
    format!("{}:{:02}:{:02}.{:02}", hours, minutes, secs, centis)
}

/// 简短显示时间: 01:23
pub fn format_duration_short(seconds: f64) -> String {
    let total_secs = seconds.max(0.0).round() as u64;
    let mins = total_secs / 60;
    let secs = total_secs % 60;
    format!("{:02}:{:02}", mins, secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_srt_time() {
        assert_eq!(seconds_to_srt_time(0.0), "00:00:00,000");
        assert_eq!(seconds_to_srt_time(65.123), "00:01:05,123");
        assert_eq!(seconds_to_srt_time(3661.005), "01:01:01,005");
    }

    #[test]
    fn test_ass_time() {
        assert_eq!(seconds_to_ass_time(0.0), "0:00:00.00");
        assert_eq!(seconds_to_ass_time(65.126), "0:01:05.13");
    }
}
