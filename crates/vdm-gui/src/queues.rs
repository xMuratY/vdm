//! Queues in the app: the editor form, names and one-line summaries.
//!
//! Plain Rust (no iced), so it is unit-tested.

use vdm_proto::{Queue, QueueId, QueueSpec, ScheduleSpec, WhenDone};

use crate::i18n::t;

/// Most downloads a queue may run at once (same cap as the global setting).
const MAX_AT_ONCE: u32 = 16;

pub const WHEN_DONE: [WhenDone; 5] =
    [WhenDone::Nothing, WhenDone::Exit, WhenDone::Sleep, WhenDone::Hibernate, WhenDone::ShutDown];

pub fn when_done_label(action: WhenDone) -> String {
    match action {
        WhenDone::Nothing => t!("when_done.nothing"),
        WhenDone::Exit => t!("when_done.exit"),
        WhenDone::Sleep => t!("when_done.sleep"),
        WhenDone::Hibernate => t!("when_done.hibernate"),
        WhenDone::ShutDown => t!("when_done.shut_down"),
    }
}

/// Short weekday names, Monday first.
pub fn day_names() -> [String; 7] {
    [
        t!("days.mon"),
        t!("days.tue"),
        t!("days.wed"),
        t!("days.thu"),
        t!("days.fri"),
        t!("days.sat"),
        t!("days.sun"),
    ]
}

/// The name to show: the main queue's is translated.
pub fn name(queue: &Queue) -> String {
    if queue.id == QueueId::MAIN { t!("queues.main") } else { queue.spec.name.clone() }
}

/// One line about how a queue runs, e.g. "Runs 23:00–07:00 on Mon, Fri. Then: shut down."
pub fn summary(queue: &Queue) -> String {
    let mut text = if queue.stopped {
        t!("queues.stopped")
    } else {
        match &queue.spec.schedule {
            None => t!("queues.any_time"),
            Some(s) => {
                let window = t!(
                    "queues.window",
                    start = s.start.as_deref().unwrap_or("00:00"),
                    stop = s.stop.as_deref().unwrap_or("24:00"),
                );
                let names = day_names();
                let days = if s.days.is_empty() || s.days.len() == 7 {
                    t!("queues.every_day")
                } else {
                    let list: Vec<&str> = s
                        .days
                        .iter()
                        .filter_map(|&d| names.get(usize::from(d)).map(String::as_str))
                        .collect();
                    t!("queues.on_days", days = list.join(", "))
                };
                format!("{window} {days}")
            }
        }
    };
    if !queue.stopped && queue.spec.schedule.is_some() && !queue.in_window {
        text = format!("{text} {}", t!("queues.waiting"));
    }
    let then = match queue.spec.when_done {
        WhenDone::Nothing => None,
        WhenDone::Exit => Some(t!("queues.then_exit")),
        WhenDone::Sleep => Some(t!("queues.then_sleep")),
        WhenDone::Hibernate => Some(t!("queues.then_hibernate")),
        WhenDone::ShutDown => Some(t!("queues.then_shut_down")),
    };
    match then {
        Some(then) => format!("{text} {then}"),
        None => text,
    }
}

/// The queue editor's fields, as typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueForm {
    pub name: String,
    pub max_active: String,
    pub speed_limit: String,
    pub scheduled: bool,
    pub start: String,
    pub stop: String,
    /// Monday first.
    pub days: [bool; 7],
    pub when_done: WhenDone,
}

impl Default for QueueForm {
    fn default() -> Self {
        Self {
            name: String::new(),
            max_active: String::new(),
            speed_limit: String::new(),
            scheduled: false,
            start: "01:00".into(),
            stop: "07:00".into(),
            days: [true; 7],
            when_done: WhenDone::Nothing,
        }
    }
}

impl QueueForm {
    pub fn from_queue(queue: &Queue) -> Self {
        let spec = &queue.spec;
        let mut form = Self {
            name: spec.name.clone(),
            max_active: spec.max_active.map(|n| n.to_string()).unwrap_or_default(),
            speed_limit: spec.speed_limit.map(crate::settings::format_speed).unwrap_or_default(),
            when_done: spec.when_done,
            ..Self::default()
        };
        if let Some(s) = &spec.schedule {
            form.scheduled = true;
            form.start = s.start.clone().unwrap_or_default();
            form.stop = s.stop.clone().unwrap_or_default();
            if !s.days.is_empty() {
                form.days = [false; 7];
                for &d in &s.days {
                    if let Some(day) = form.days.get_mut(usize::from(d)) {
                        *day = true;
                    }
                }
            }
        }
        form
    }

    /// The settings to save, or a message naming the first bad field.
    pub fn parse(&self, is_main: bool) -> Result<QueueSpec, String> {
        let name = if is_main { "Main".to_owned() } else { self.name.trim().to_owned() };
        if name.is_empty() {
            return Err(t!("queue.err_name"));
        }
        let max_active = match self.max_active.trim() {
            "" => None,
            text => Some(
                text.parse::<u32>()
                    .ok()
                    .filter(|n| (1..=MAX_AT_ONCE).contains(n))
                    .ok_or_else(|| t!("queue.err_at_once", max = MAX_AT_ONCE))?,
            ),
        };
        let speed_limit = match self.speed_limit.trim() {
            "" => None,
            text => Some(
                crate::settings::parse_speed(text)
                    .map_err(|err| t!("settings.err_speed", error = err))?,
            ),
        };
        let schedule = if self.scheduled {
            let start = parse_time(&self.start).ok_or_else(|| t!("queue.err_time"))?;
            let stop = parse_time(&self.stop).ok_or_else(|| t!("queue.err_time"))?;
            let picked: Vec<u8> = (0u8..7).filter(|&d| self.days[usize::from(d)]).collect();
            if picked.is_empty() {
                return Err(t!("queue.err_days"));
            }
            // All seven means every day.
            let days = if picked.len() == 7 { Vec::new() } else { picked };
            Some(ScheduleSpec { start, stop, days })
        } else {
            None
        };
        Ok(QueueSpec { name, max_active, speed_limit, schedule, when_done: self.when_done })
    }
}

/// "7:5" -> "07:05"; blank -> None (midnight); anything else invalid.
fn parse_time(text: &str) -> Option<Option<String>> {
    let text = text.trim();
    if text.is_empty() {
        return Some(None);
    }
    let (h, m) = text.split_once(':')?;
    let (h, m) = (h.trim().parse::<u32>().ok()?, m.trim().parse::<u32>().ok()?);
    (h < 24 && m < 60).then(|| Some(format!("{h:02}:{m:02}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queue(spec: QueueSpec) -> Queue {
        Queue { id: QueueId(2), spec, stopped: false, in_window: true }
    }

    fn spec() -> QueueSpec {
        QueueForm { name: "Night".into(), ..QueueForm::default() }.parse(false).unwrap()
    }

    #[test]
    fn a_plain_form_parses() {
        let s = spec();
        assert_eq!(s.name, "Night");
        assert_eq!((s.max_active, s.speed_limit, s.schedule), (None, None, None));
        assert_eq!(s.when_done, WhenDone::Nothing);
    }

    #[test]
    fn schedules_parse_and_round_trip() {
        let mut form = QueueForm {
            name: " Night ".into(),
            max_active: "2".into(),
            speed_limit: "1M".into(),
            scheduled: true,
            start: "23:0".into(),
            stop: "7:30".into(),
            when_done: WhenDone::ShutDown,
            ..QueueForm::default()
        };
        form.days = [true, false, false, false, true, false, false];
        let s = form.parse(false).unwrap();
        assert_eq!(s.max_active, Some(2));
        assert_eq!(s.speed_limit, Some(1 << 20));
        let schedule = s.schedule.clone().unwrap();
        assert_eq!(schedule.start.as_deref(), Some("23:00"));
        assert_eq!(schedule.stop.as_deref(), Some("07:30"));
        assert_eq!(schedule.days, vec![0, 4]);

        let back = QueueForm::from_queue(&queue(s.clone()));
        assert_eq!(back.parse(false).unwrap(), s);
    }

    #[test]
    fn every_day_is_stored_as_no_days() {
        let form = QueueForm { name: "x".into(), scheduled: true, ..QueueForm::default() };
        assert_eq!(form.parse(false).unwrap().schedule.unwrap().days, Vec::<u8>::new());
    }

    #[test]
    fn bad_fields_are_reported() {
        let named = |f: QueueForm| QueueForm { name: "x".into(), ..f };
        assert!(QueueForm::default().parse(false).is_err(), "no name");
        assert!(QueueForm::default().parse(true).is_ok(), "the main queue keeps its name");
        assert!(
            named(QueueForm { max_active: "0".into(), ..QueueForm::default() })
                .parse(false)
                .is_err()
        );
        assert!(
            named(QueueForm { max_active: "99".into(), ..QueueForm::default() })
                .parse(false)
                .is_err()
        );
        assert!(
            named(QueueForm { speed_limit: "fast".into(), ..QueueForm::default() })
                .parse(false)
                .is_err()
        );
        let bad_time = QueueForm { scheduled: true, start: "25:00".into(), ..QueueForm::default() };
        assert!(named(bad_time).parse(false).is_err());
        let no_days = QueueForm { scheduled: true, days: [false; 7], ..QueueForm::default() };
        assert!(named(no_days).parse(false).is_err());
    }

    #[test]
    fn times_are_normalized() {
        assert_eq!(parse_time(""), Some(None));
        assert_eq!(parse_time("9:5"), Some(Some("09:05".into())));
        assert_eq!(parse_time("24:00"), None);
        assert_eq!(parse_time("noon"), None);
    }

    #[test]
    fn summaries_describe_the_queue() {
        assert_eq!(summary(&queue(spec())), "Runs at any time.");
        let mut q = queue(QueueSpec {
            schedule: Some(ScheduleSpec {
                start: Some("23:00".into()),
                stop: Some("07:00".into()),
                days: vec![0, 4],
            }),
            when_done: WhenDone::ShutDown,
            ..spec()
        });
        assert_eq!(summary(&q), "Runs 23:00–07:00 on Mon, Fri. Then the computer shuts down.");
        q.in_window = false;
        assert!(summary(&q).contains("Waiting for its time"), "{}", summary(&q));
        q.stopped = true;
        assert!(summary(&q).starts_with("Stopped."), "{}", summary(&q));
    }
}
