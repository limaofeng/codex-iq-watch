//! 时间戳渲染：不引入日期库，直接按 UTC 秒数展开公历。

/// Unix 毫秒 → RFC3339（UTC，`Z` 结尾）。仅用于展示与通知正文。
#[must_use]
pub fn to_rfc3339(ms: u64) -> String {
    let seconds = ms / 1000;
    let millis = ms % 1000;
    let days = seconds / 86_400;
    let day_seconds = seconds % 86_400;
    let hour = day_seconds / 3600;
    let minute = (day_seconds % 3600) / 60;
    let second = day_seconds % 60;
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// Howard Hinnant 公历算法：天数（自 1970-01-01）→ (y, m, d)。
fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::to_rfc3339;

    #[test]
    fn formats_epoch_and_known_date() {
        assert_eq!(to_rfc3339(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(to_rfc3339(86_400_000), "1970-01-02T00:00:00.000Z");
        // 2026-09-26T00:00:00Z = 20722 天。
        assert_eq!(to_rfc3339(1_790_380_800_000), "2026-09-26T00:00:00.000Z");
    }
}
