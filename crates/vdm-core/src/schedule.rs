//! Daily time windows during which a queue may download.
//!
//! A [`Schedule`] is a set of windows that repeat daily, optionally limited to
//! certain weekdays. A window that starts on day `D` may run past midnight.
//! Times are naive local times, with no time zone or DST handling.

use chrono::{Datelike, NaiveDate, NaiveDateTime, NaiveTime, Weekday};

/// A daily time window during which a queue may download.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Schedule {
    /// Start of the daily window. `None` = midnight (00:00).
    pub start: Option<NaiveTime>,
    /// End of the daily window. `None` = the following midnight.
    pub stop: Option<NaiveTime>,
    /// Days on which a window STARTS. Empty = every day.
    pub days: Vec<Weekday>,
}

impl Schedule {
    /// No restriction: always active.
    pub fn always() -> Self {
        Self::default()
    }

    /// Build from a queue's settings: "HH:MM" times, days 0 = Monday … 6 = Sunday.
    pub fn from_spec(spec: &vdm_proto::ScheduleSpec) -> Result<Self, String> {
        let time = |text: &Option<String>| -> Result<Option<NaiveTime>, String> {
            match text.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
                None => Ok(None),
                Some(t) => NaiveTime::parse_from_str(t, "%H:%M")
                    .map(Some)
                    .map_err(|_| format!("`{t}` is not a time like 07:30")),
            }
        };
        let days = spec
            .days
            .iter()
            .map(|&d| {
                Weekday::try_from(d).map_err(|_| format!("{d} is not a weekday number (0-6)"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { start: time(&spec.start)?, stop: time(&spec.stop)?, days })
    }

    /// Whether downloading is allowed at local time `now`.
    pub fn is_active(&self, now: NaiveDateTime) -> bool {
        let today = now.date();
        today
            .pred_opt()
            .into_iter()
            .chain(Some(today))
            .filter_map(|day| self.window_on(day))
            .any(|(start, end)| start <= now && now < end)
    }

    /// The next moment strictly after `now` at which `is_active` changes value,
    /// or `None` if it never changes (always or never active).
    pub fn next_change(&self, now: NaiveDateTime) -> Option<NaiveDateTime> {
        // Windows that start on the day before `now` may still cover it. Ten
        // days (yesterday through eight days ahead) include at least one start
        // for any weekday filter.
        let first = now.date().pred_opt()?;
        let mut bounds: Vec<NaiveDateTime> = std::iter::successors(Some(first), |d| d.succ_opt())
            .take(10)
            .filter_map(|day| self.window_on(day))
            .flat_map(|(start, end)| [start, end])
            .collect();
        bounds.sort_unstable();

        let active = self.is_active(now);
        bounds.into_iter().find(|&b| b > now && self.is_active(b) != active)
    }

    /// The `[start, end)` window that starts on `day`, if a window starts then.
    fn window_on(&self, day: NaiveDate) -> Option<(NaiveDateTime, NaiveDateTime)> {
        if !self.days.is_empty() && !self.days.contains(&day.weekday()) {
            return None;
        }
        let start = self.start.unwrap_or(NaiveTime::MIN);
        let next_day = day.succ_opt()?;
        let end = match self.stop {
            None => next_day.and_time(NaiveTime::MIN),
            // The window stays inside `day`.
            Some(stop) if stop > start => day.and_time(stop),
            // `stop <= start`, including equal: the window wraps past midnight.
            Some(stop) => next_day.and_time(stop),
        };
        Some((day.and_time(start), end))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parses `"YYYY-MM-DD HH:MM"`.
    fn dt(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").expect("valid datetime")
    }

    /// Parses `"HH:MM"`.
    fn t(s: &str) -> NaiveTime {
        NaiveTime::parse_from_str(s, "%H:%M").expect("valid time")
    }

    fn window(start: Option<&str>, stop: Option<&str>, days: &[Weekday]) -> Schedule {
        Schedule { start: start.map(t), stop: stop.map(t), days: days.to_vec() }
    }

    #[test]
    fn from_spec_parses_times_and_days() {
        let spec = vdm_proto::ScheduleSpec {
            start: Some("23:00".into()),
            stop: Some(" 07:30 ".into()),
            days: vec![0, 4],
        };
        let s = Schedule::from_spec(&spec).unwrap();
        assert_eq!(s, window(Some("23:00"), Some("07:30"), &[Weekday::Mon, Weekday::Fri]));
        let empty = vdm_proto::ScheduleSpec { start: Some(String::new()), ..Default::default() };
        assert_eq!(Schedule::from_spec(&empty).unwrap(), Schedule::always());
        let bad_time = vdm_proto::ScheduleSpec { stop: Some("25:00".into()), ..Default::default() };
        assert!(Schedule::from_spec(&bad_time).is_err());
        let bad_day = vdm_proto::ScheduleSpec { days: vec![7], ..Default::default() };
        assert!(Schedule::from_spec(&bad_day).is_err());
    }

    #[test]
    fn always_is_active_and_never_changes() {
        let s = Schedule::always();
        for now in ["2026-10-23 00:00", "2026-10-23 12:34", "2026-10-24 23:59"] {
            assert!(s.is_active(dt(now)), "{now}");
        }
        assert_eq!(s.next_change(dt("2026-10-23 12:00")), None);
    }

    #[test]
    fn simple_window_9_to_17() {
        let s = window(Some("09:00"), Some("17:00"), &[]);
        assert!(!s.is_active(dt("2026-10-23 08:59")));
        assert!(s.is_active(dt("2026-10-23 09:00")));
        assert!(s.is_active(dt("2026-10-23 16:59")));
        assert!(!s.is_active(dt("2026-10-23 17:00")));

        assert_eq!(s.next_change(dt("2026-10-23 08:00")), Some(dt("2026-10-23 09:00")));
        assert_eq!(s.next_change(dt("2026-10-23 12:00")), Some(dt("2026-10-23 17:00")));
        assert_eq!(s.next_change(dt("2026-10-23 17:00")), Some(dt("2026-10-24 09:00")));
    }

    #[test]
    fn wrap_23_to_7() {
        let s = window(Some("23:00"), Some("07:00"), &[]);
        assert!(s.is_active(dt("2026-10-23 23:30")));
        assert!(s.is_active(dt("2026-10-24 03:00")));
        assert!(!s.is_active(dt("2026-10-24 07:00")));
        assert!(!s.is_active(dt("2026-10-24 12:00")));

        assert_eq!(s.next_change(dt("2026-10-23 12:00")), Some(dt("2026-10-23 23:00")));
        assert_eq!(s.next_change(dt("2026-10-23 23:30")), Some(dt("2026-10-24 07:00")));
    }

    #[test]
    fn weekday_filter_mon_to_fri_9_to_17() {
        let weekdays = [Weekday::Mon, Weekday::Tue, Weekday::Wed, Weekday::Thu, Weekday::Fri];
        let s = window(Some("09:00"), Some("17:00"), &weekdays);
        // 2026-10-24 is a Saturday.
        assert!(!s.is_active(dt("2026-10-24 10:00")));
        assert_eq!(s.next_change(dt("2026-10-23 17:00")), Some(dt("2026-10-26 09:00")));
    }

    #[test]
    fn wrap_with_day_filter_uses_start_day() {
        let s = window(Some("23:00"), Some("07:00"), &[Weekday::Fri]);
        // Friday 2026-10-23 and Saturday 2026-10-24.
        assert!(s.is_active(dt("2026-10-23 23:30")));
        assert!(s.is_active(dt("2026-10-24 03:00")));
        assert!(!s.is_active(dt("2026-10-24 23:30")));
        // Thursday 2026-10-22.
        assert!(!s.is_active(dt("2026-10-22 23:30")));
    }

    #[test]
    fn start_equals_stop_is_full_day() {
        let s = window(Some("08:00"), Some("08:00"), &[]);
        assert!(s.is_active(dt("2026-10-23 08:00")));
        assert!(s.is_active(dt("2026-10-23 07:59")));
        assert_eq!(s.next_change(dt("2026-10-23 12:00")), None);
    }

    #[test]
    fn start_equals_stop_on_one_day_wraps_to_next_day() {
        let s = window(Some("08:00"), Some("08:00"), &[Weekday::Mon]);
        // 2026-10-19 is a Monday.
        assert!(s.is_active(dt("2026-10-19 08:00")));
        assert!(s.is_active(dt("2026-10-20 07:59")));
        assert!(!s.is_active(dt("2026-10-20 08:00")));
    }

    #[test]
    fn start_only_runs_until_midnight() {
        let s = window(Some("20:00"), None, &[]);
        assert!(!s.is_active(dt("2026-10-23 19:59")));
        assert!(s.is_active(dt("2026-10-23 20:00")));
        assert!(s.is_active(dt("2026-10-23 23:59")));
        assert!(!s.is_active(dt("2026-10-24 00:00")));
    }

    #[test]
    fn stop_only_runs_from_midnight() {
        let s = window(None, Some("06:00"), &[]);
        assert!(s.is_active(dt("2026-10-23 00:00")));
        assert!(s.is_active(dt("2026-10-23 05:59")));
        assert!(!s.is_active(dt("2026-10-23 06:00")));
    }

    #[test]
    fn adjacent_full_days_do_not_report_a_change_at_midnight() {
        let s = window(None, None, &[Weekday::Mon, Weekday::Tue]);
        // 2026-10-19 is a Monday, 2026-10-20 a Tuesday, 2026-10-21 a Wednesday.
        assert!(s.is_active(dt("2026-10-19 00:00")));
        assert!(s.is_active(dt("2026-10-20 23:59")));
        assert!(!s.is_active(dt("2026-10-21 00:00")));
        assert_eq!(s.next_change(dt("2026-10-19 12:00")), Some(dt("2026-10-21 00:00")));
    }

    #[test]
    fn next_change_finds_a_weekly_window_within_range() {
        // Only Sunday. From Monday noon, the next start is six days away.
        let s = window(Some("09:00"), Some("10:00"), &[Weekday::Sun]);
        // 2026-10-26 is a Monday, 2026-10-25 a Sunday.
        assert_eq!(s.next_change(dt("2026-10-26 12:00")), Some(dt("2026-11-01 09:00")));
    }
}
