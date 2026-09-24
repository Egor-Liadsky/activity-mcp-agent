//! Время: секунды Unix в базе, локальное время в текстах и расписании.

use chrono::{DateTime, Local, NaiveDate, TimeZone, Utc};
use std::time::Duration;

pub fn now() -> i64 {
    Utc::now().timestamp()
}

/// Длительность вида `90s`, `15m`, `6h`, `7d`, `2w`.
pub fn parse_duration(text: &str) -> Result<Duration, String> {
    let text = text.trim();
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .ok_or_else(|| format!("у длительности «{text}» нет единицы: s, m, h, d или w"))?;
    let (number, unit) = text.split_at(split);
    let number: u64 = number
        .parse()
        .map_err(|_| format!("длительность «{text}» должна начинаться с числа"))?;
    let seconds = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        "w" => 7 * 86_400,
        other => return Err(format!("неизвестная единица «{other}» в «{text}»: s, m, h, d или w")),
    };
    Ok(Duration::from_secs(number * seconds))
}

/// Начало периода: относительное (`24h`, `7d` — столько назад от `now`),
/// дата `YYYY-MM-DD` (местная полночь) или RFC 3339.
pub fn parse_since(text: &str, now: i64) -> Result<i64, String> {
    let text = text.trim();
    if let Ok(duration) = parse_duration(text) {
        return Ok(now - duration.as_secs() as i64);
    }
    if let Ok(moment) = DateTime::parse_from_rfc3339(text) {
        return Ok(moment.timestamp());
    }
    if let Ok(date) = NaiveDate::parse_from_str(text, "%Y-%m-%d") {
        let midnight = date.and_hms_opt(0, 0, 0).expect("полночь существует");
        return Local
            .from_local_datetime(&midnight)
            .earliest()
            .map(|moment| moment.timestamp())
            .ok_or_else(|| format!("нет местной полуночи для {text}"));
    }
    Err(format!(
        "не разобрано «{text}»: ожидается 24h, 7d, 2026-09-01 или 2026-09-01T09:00:00+03:00"
    ))
}

/// Для JSON-ответов: RFC 3339 в местном поясе.
pub fn rfc3339(timestamp: i64) -> String {
    local(timestamp).to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
}

/// Для текста сводки: `24.09 18:00`.
pub fn short(timestamp: i64) -> String {
    local(timestamp).format("%d.%m %H:%M").to_string()
}

fn local(timestamp: i64) -> DateTime<Local> {
    Local
        .timestamp_opt(timestamp, 0)
        .single()
        .unwrap_or_else(|| Local.timestamp_opt(0, 0).single().expect("эпоха"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_are_parsed() {
        assert_eq!(parse_duration("90s"), Ok(Duration::from_secs(90)));
        assert_eq!(parse_duration("15m"), Ok(Duration::from_secs(900)));
        assert_eq!(parse_duration("2w"), Ok(Duration::from_secs(14 * 86_400)));
        assert!(parse_duration("15").is_err());
        assert!(parse_duration("m").is_err());
        assert!(parse_duration("3y").is_err());
    }

    #[test]
    fn since_accepts_relative_and_absolute() {
        assert_eq!(parse_since("24h", 100_000), Ok(100_000 - 86_400));
        assert_eq!(parse_since("2026-09-01T09:00:00+00:00", 0), Ok(1_788_253_200));
        assert!(parse_since("2026-09-01", 0).is_ok());
        assert!(parse_since("вчера", 0).is_err());
    }
}
