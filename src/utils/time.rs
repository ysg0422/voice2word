//! 时间戳转换与格式化工具

/// 把浮点秒换算成「从小到大各时间单位」的整数，供各格式化函数共用。
///
/// 负秒钳到 0；超过 `99:59:59` 的钳到该上限（理由见 [`seconds_to_srt_time`]）。
/// 钳位作用在**整数刻度**上而不是秒上，否则「钳到 359999.999 秒再按 0.1 秒取整」
/// 会进位成整整 100 小时，照样把两位的 `HH` 撑破。
/// NaN 经 `max` 落到 0；`as u64` 是饱和转换，不会 panic。
fn split_time(seconds: f64, units_per_sec: f64) -> (u64, u64, u64, u64) {
    let ups = units_per_sec as u64;
    let per_hour = ups * 3600;
    let total = ((seconds.max(0.0) * units_per_sec).round() as u64).min(per_hour * 100 - 1);
    (
        total / per_hour,
        (total % per_hour) / (ups * 60),
        (total % (ups * 60)) / ups,
        total % ups,
    )
}

/// 将浮点秒转换为 SRT 时间格式: 00:01:23,456
///
/// 负值钳到 0、超过 99:59:59.999 的钳到上限：`HH` 只有两位，一旦超过 99 小时，
/// 时间码就会写成 3 位以上并破坏 SRT 的 `HH:MM:SS,mmm` 结构——单个字幕文件
/// 不可能有 100 小时，出现这种值只说明输入本身异常。
pub fn seconds_to_srt_time(seconds: f64) -> String {
    let (hours, minutes, secs, millis) = split_time(seconds, 1000.0);
    format!("{:02}:{:02}:{:02},{:03}", hours, minutes, secs, millis)
}

/// 将浮点秒转换为标准毫秒时间戳格式: 00:01:23.456
pub fn seconds_to_timestamp(seconds: f64) -> String {
    let (hours, minutes, secs, millis) = split_time(seconds, 1000.0);
    format!("{:02}:{:02}:{:02}.{:03}", hours, minutes, secs, millis)
}

/// 将浮点秒转换为 ASS 时间格式: 0:01:23.45 (两位小数百分秒)
pub fn seconds_to_ass_time(seconds: f64) -> String {
    let (hours, minutes, secs, centis) = split_time(seconds, 100.0);
    format!("{}:{:02}:{:02}.{:02}", hours, minutes, secs, centis)
}

/// 界面展示用的紧凑时间戳: 00:01:23.4（精确到 0.1 秒）
///
/// 只用于字幕清单 / 快速编辑卡这类**屏幕展示**，导出仍走 [`seconds_to_timestamp`]
/// 的毫秒精度——导出是数据，界面是观感，两者精度需求不同：毫秒后缀（`.456`）
/// 扫读时几乎无意义，却把时间列撑宽，挤掉原文与译文的空间。
pub fn seconds_to_timestamp_short(seconds: f64) -> String {
    let (hours, minutes, secs, tenths) = split_time(seconds, 10.0);
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

/// 文件名安全的本地时间戳：`20261007_153012`。
///
/// 用于给「损坏数据库备份」等需要唯一命名的文件加后缀——冒号、空格在 Windows
/// 文件名里非法，故只留数字与下划线。
pub fn timestamp_for_filename() -> String {
    chrono::Local::now().format("%Y%m%d_%H%M%S").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filename_timestamp_has_no_illegal_chars() {
        let t = timestamp_for_filename();
        // 形如 20261007_153012：15 个字符，仅数字与下划线
        assert_eq!(t.len(), 15, "unexpected: {t}");
        assert!(
            t.chars().all(|c| c.is_ascii_digit() || c == '_'),
            "unexpected: {t}"
        );
    }

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

    /// 毫秒进位：59.9995s 四舍五入到 60.000s，必须进位成 `00:01:00,000`，
    /// 而不是写成 `00:00:60,000` 这种非法时间码。
    #[test]
    fn millisecond_rounding_carries_into_next_second() {
        assert_eq!(seconds_to_srt_time(59.9995), "00:01:00,000");
        assert_eq!(seconds_to_timestamp(59.9995), "00:01:00.000");
        // 分/时同样要跟着进位
        assert_eq!(seconds_to_srt_time(3599.9999), "01:00:00,000");
        // ASS 是百分秒：0.9995 秒进位成 1.00
        assert_eq!(seconds_to_ass_time(0.9995), "0:00:01.00");
    }

    /// 负值归零、超 99 小时钳位：`HH` 只有两位，溢出会破坏时间码结构。
    #[test]
    fn clamps_negative_and_overlong_times() {
        assert_eq!(seconds_to_srt_time(-1.0), "00:00:00,000");
        assert_eq!(seconds_to_timestamp(-0.5), "00:00:00.000");
        assert_eq!(seconds_to_ass_time(-100.0), "0:00:00.00");
        // 恰好 100 小时 → 钳到 99:59:59.999，绝不能写出 100:00:00.000
        assert_eq!(seconds_to_srt_time(360_000.0), "99:59:59,999");
        assert_eq!(seconds_to_timestamp(1e12), "99:59:59.999");
        assert_eq!(seconds_to_timestamp_short(1e12), "99:59:59.9");
        // NaN 不应 panic，也不应产出怪异时间码
        assert_eq!(seconds_to_srt_time(f64::NAN), "00:00:00,000");
        assert_eq!(seconds_to_srt_time(f64::INFINITY), "99:59:59,999");
    }
}
