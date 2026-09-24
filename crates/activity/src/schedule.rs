//! Расписание сводок: cron-выражение в местном времени.

use chrono::{DateTime, Local, TimeZone};
use std::str::FromStr;

#[derive(Debug, Clone)]
pub struct Schedule {
    cron: cron::Schedule,
    expression: String,
}

impl Schedule {
    /// Принимает и классические пять полей (`0 9,18 * * *`), и шесть-семь
    /// полей крейта `cron` с секундами впереди (`0 0 9,18 * * *`).
    pub fn parse(expression: &str) -> Result<Self, String> {
        let expression = expression.trim();
        let full = if expression.split_whitespace().count() == 5 {
            format!("0 {expression}")
        } else {
            expression.to_string()
        };
        let cron = cron::Schedule::from_str(&full)
            .map_err(|err| format!("неверное расписание «{expression}»: {err}"))?;
        Ok(Self {
            cron,
            expression: expression.to_string(),
        })
    }

    pub fn expression(&self) -> &str {
        &self.expression
    }

    /// Ближайший запуск строго после `after` (секунды Unix).
    pub fn next_after(&self, after: i64) -> Option<i64> {
        let after: DateTime<Local> = Local.timestamp_opt(after, 0).single()?;
        self.cron.after(&after).next().map(|moment| moment.timestamp())
    }

    /// Был ли запуск в `(last, now]`, который демон проспал (был выключен
    /// или машина спала). Без прошлой сводки пропуска нет: первая сводка —
    /// по расписанию.
    pub fn missed(&self, last: Option<i64>, now: i64) -> bool {
        last.and_then(|last| self.next_after(last))
            .is_some_and(|next| next <= now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(hour: u32, minute: u32) -> i64 {
        Local
            .with_ymd_and_hms(2026, 9, 24, hour, minute, 0)
            .single()
            .unwrap()
            .timestamp()
    }

    #[test]
    fn five_and_six_fields_mean_the_same() {
        let five = Schedule::parse("0 9,18 * * *").unwrap();
        let six = Schedule::parse("0 0 9,18 * * *").unwrap();
        assert_eq!(five.next_after(at(10, 0)), Some(at(18, 0)));
        assert_eq!(six.next_after(at(10, 0)), Some(at(18, 0)));
        assert!(Schedule::parse("каждый час").is_err());
    }

    #[test]
    fn missed_run_is_detected() {
        let schedule = Schedule::parse("0 9,18 * * *").unwrap();
        // Последняя сводка в 9:00, сейчас 19:00 — запуск в 18:00 пропущен.
        assert!(schedule.missed(Some(at(9, 0)), at(19, 0)));
        assert!(!schedule.missed(Some(at(9, 0)), at(17, 59)));
        assert!(!schedule.missed(None, at(19, 0)));
    }
}
