//! EventBridge Scheduler expressions in plain English, and their next run
//! times. Schedules store `cron(min hour day-of-month month day-of-week
//! year)` in UTC, where day-of-week runs 1-7 from **Sunday** (not Unix's
//! 0 = Sunday), one of the two day fields must be `?`, and `a/b` means
//! "from a, every b, within the field's range". Hand-written because the
//! common crates use Unix or Quartz numbering, an easy off-by-one weekday.

use std::fmt;

const MONTH_NAMES: [&str; 12] = [
    "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC",
];
const MONTH_LABELS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
const DAY_NAMES: [&str; 7] = ["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"];
const DAY_LABELS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];
const ORDINALS: [&str; 5] = ["first", "second", "third", "fourth", "fifth"];
const MS_PER_MINUTE: i64 = 60_000;
const MS_PER_DAY: i64 = 86_400_000;
/// How far ahead next-run search looks before giving up (about two years).
const SEARCH_DAYS: i64 = 800;
/// AWS Scheduler's year range.
const YEARS: (u32, u32) = (1970, 2199);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronError(String);

impl fmt::Display for CronError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

fn error<T>(text: impl Into<String>) -> Result<T, CronError> {
    Err(CronError(text.into()))
}

/// One comma-separated part of a field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Part {
    Value(u32),
    Range(u32, u32),
    /// `from/step` (to the field's end) or `from-to/step`.
    Step {
        from: u32,
        to: u32,
        step: u32,
    },
}

/// A field's allowed values; `any` for `*` and `?`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Field {
    any: bool,
    parts: Vec<Part>,
    min: u32,
    max: u32,
}

impl Field {
    fn matches(&self, value: u32) -> bool {
        self.any
            || self.parts.iter().any(|part| match *part {
                Part::Value(v) => v == value,
                Part::Range(a, b) => (a..=b).contains(&value),
                Part::Step { from, to, step } => {
                    (from..=to).contains(&value) && (value - from) % step == 0
                }
            })
    }

    fn values(&self) -> Vec<u32> {
        (self.min..=self.max).filter(|v| self.matches(*v)).collect()
    }

    fn single(&self) -> Option<u32> {
        match self.parts.as_slice() {
            [Part::Value(v)] if !self.any => Some(*v),
            _ => None,
        }
    }

    /// Only plain values (no ranges or steps), e.g. `MON,WED`.
    fn list(&self) -> Option<Vec<u32>> {
        if self.any {
            return None;
        }
        self.parts
            .iter()
            .map(|p| match p {
                Part::Value(v) => Some(*v),
                _ => None,
            })
            .collect()
    }
}

/// Day-of-month extras.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MonthDay {
    Last,
    LastWeekday,
    NearestWeekday(u32),
}

/// Day-of-week extras.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WeekDay {
    /// `6L`: the last Friday of the month; plain `L` is the last Saturday.
    Last(u32),
    /// `2#1`: the first Monday of the month.
    Nth(u32, u32),
}

/// A parsed `cron(...)` expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cron {
    minutes: Field,
    hours: Field,
    month_days: Field,
    month_day: Option<MonthDay>,
    months: Field,
    week_days: Field,
    week_day: Option<WeekDay>,
    years: Field,
}

/// What a schedule's expression can be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expression {
    Cron(Box<Cron>),
    /// `rate(5 minutes)`: amount and unit as written.
    Rate(u32, String),
    /// `at(2026-05-26T07:10:00)`: one run at this UTC time (epoch ms).
    At(i64),
}

/// Reads `cron(...)`, `rate(...)` or `at(...)`; a bare six-field cron works too.
pub fn parse(text: &str) -> Result<Expression, CronError> {
    let text = text.trim();
    if let Some(inner) = wrapped(text, "rate") {
        return parse_rate(inner);
    }
    if let Some(inner) = wrapped(text, "at") {
        return crate::tui::format::epoch_ms(&serde_json::Value::String(inner.to_string()))
            .map(Expression::At)
            .ok_or_else(|| CronError(format!("`{inner}` is not a date and time")));
    }
    let inner = wrapped(text, "cron").unwrap_or(text);
    parse_cron(inner).map(|c| Expression::Cron(Box::new(c)))
}

fn wrapped<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let rest = text.get(..name.len())?;
    if !rest.eq_ignore_ascii_case(name) {
        return None;
    }
    text[name.len()..]
        .trim_start()
        .strip_prefix('(')?
        .strip_suffix(')')
        .map(str::trim)
}

fn parse_rate(inner: &str) -> Result<Expression, CronError> {
    let mut words = inner.split_whitespace();
    let (Some(amount), Some(unit), None) = (words.next(), words.next(), words.next()) else {
        return error("A rate is written like rate(5 minutes)");
    };
    let amount = amount
        .parse()
        .map_err(|_| CronError(format!("`{amount}` is not a number")))?;
    Ok(Expression::Rate(amount, unit.to_lowercase()))
}

pub fn parse_cron(inner: &str) -> Result<Cron, CronError> {
    let fields: Vec<&str> = inner.split_whitespace().collect();
    let [minutes, hours, month_days, months, week_days, years] = fields[..] else {
        return error(format!(
            "A cron needs 6 fields (minute hour day month weekday year), not {}",
            fields.len()
        ));
    };
    let (month_days, month_day) = parse_month_days(month_days)?;
    let (week_days, week_day) = parse_week_days(week_days)?;
    let month_days_open = month_days.any && month_day.is_none();
    let week_days_open = week_days.any && week_day.is_none();
    if !month_days_open && !week_days_open {
        return error("Use ? in either the day-of-month or the day-of-week field");
    }
    if !(fields[2] == "?" || fields[4] == "?") {
        return error("Use ? in either the day-of-month or the day-of-week field");
    }
    Ok(Cron {
        minutes: parse_field(minutes, "minute", 0, 59, &[])?,
        hours: parse_field(hours, "hour", 0, 23, &[])?,
        month_days,
        month_day,
        months: parse_field(months, "month", 1, 12, &MONTH_NAMES)?,
        week_days,
        week_day,
        years: parse_field(years, "year", YEARS.0, YEARS.1, &[])?,
    })
}

fn parse_month_days(text: &str) -> Result<(Field, Option<MonthDay>), CronError> {
    let upper = text.to_ascii_uppercase();
    let special = match upper.as_str() {
        "L" => Some(MonthDay::Last),
        "LW" => Some(MonthDay::LastWeekday),
        _ => match upper.strip_suffix('W') {
            Some(day) => Some(MonthDay::NearestWeekday(number(day, "day", 1, 31, &[])?)),
            None => None,
        },
    };
    match special {
        Some(special) => Ok((any_field(1, 31), Some(special))),
        None => Ok((parse_field(text, "day", 1, 31, &[])?, None)),
    }
}

fn parse_week_days(text: &str) -> Result<(Field, Option<WeekDay>), CronError> {
    let upper = text.to_ascii_uppercase();
    if upper == "L" {
        return Ok((any_field(1, 7), Some(WeekDay::Last(7))));
    }
    if let Some(day) = upper.strip_suffix('L') {
        let day = number(day, "weekday", 1, 7, &DAY_NAMES)?;
        return Ok((any_field(1, 7), Some(WeekDay::Last(day))));
    }
    if let Some((day, nth)) = upper.split_once('#') {
        let day = number(day, "weekday", 1, 7, &DAY_NAMES)?;
        let nth = number(nth, "week of the month", 1, 5, &[])?;
        return Ok((any_field(1, 7), Some(WeekDay::Nth(day, nth))));
    }
    Ok((parse_field(text, "weekday", 1, 7, &DAY_NAMES)?, None))
}

fn any_field(min: u32, max: u32) -> Field {
    Field {
        any: true,
        parts: Vec::new(),
        min,
        max,
    }
}

fn parse_field(
    text: &str,
    what: &str,
    min: u32,
    max: u32,
    names: &[&str],
) -> Result<Field, CronError> {
    if text == "*" || text == "?" {
        return Ok(any_field(min, max));
    }
    let parts = text
        .split(',')
        .map(|part| parse_part(part, what, min, max, names))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Field {
        any: false,
        parts,
        min,
        max,
    })
}

fn parse_part(
    part: &str,
    what: &str,
    min: u32,
    max: u32,
    names: &[&str],
) -> Result<Part, CronError> {
    if let Some((range, step)) = part.split_once('/') {
        let step = number(step, what, 1, max.max(1), &[])?;
        let (from, to) = match range {
            "*" => (min, max),
            _ => match range.split_once('-') {
                Some((a, b)) => (
                    number(a, what, min, max, names)?,
                    number(b, what, min, max, names)?,
                ),
                None => (number(range, what, min, max, names)?, max),
            },
        };
        return Ok(Part::Step { from, to, step });
    }
    match part.split_once('-') {
        Some((a, b)) => {
            let (a, b) = (
                number(a, what, min, max, names)?,
                number(b, what, min, max, names)?,
            );
            if a > b {
                return error(format!("The {what} range {part} runs backwards"));
            }
            Ok(Part::Range(a, b))
        }
        None => Ok(Part::Value(number(part, what, min, max, names)?)),
    }
}

/// A number in `min..=max`, or a name (`MON`, `jan`) counted from `min`.
fn number(text: &str, what: &str, min: u32, max: u32, names: &[&str]) -> Result<u32, CronError> {
    if let Some(index) = names.iter().position(|n| n.eq_ignore_ascii_case(text)) {
        return Ok(min + index as u32);
    }
    let value: u32 = text
        .parse()
        .map_err(|_| CronError(format!("`{text}` is not a valid {what}")))?;
    if !(min..=max).contains(&value) {
        return error(format!("The {what} {value} is outside {min}-{max}"));
    }
    Ok(value)
}

// ---------- plain English ----------

/// "every weekday at 09:00 UTC"; falls back to a field-by-field sentence.
pub fn describe(expression: &Expression) -> String {
    match expression {
        Expression::Rate(1, unit) => format!("every {}", unit.trim_end_matches('s')),
        Expression::Rate(amount, unit) => format!("every {amount} {unit}"),
        Expression::At(ms) => format!("once on {}", day_and_time(*ms)),
        Expression::Cron(cron) => describe_cron(cron),
    }
}

fn describe_cron(cron: &Cron) -> String {
    if let Some(once) = one_shot(cron) {
        return once;
    }
    let time = time_phrase(cron);
    let mut text = match day_phrase(cron) {
        Some(days) => format!("{days} {time} UTC"),
        None if time.starts_with("at ") => format!("every day {time} UTC"),
        None => format!("{time} UTC"),
    };
    if let Some(months) = month_phrase(cron) {
        text.push_str(&format!(" in {months}"));
    }
    if let Some(years) = year_phrase(cron) {
        text.push_str(&format!(", {years}"));
    }
    text
}

/// Every field fixed, year included: the web's "streaming" one-shot.
fn one_shot(cron: &Cron) -> Option<String> {
    let year = cron.years.single()?;
    let month = cron.months.single()?;
    let day = cron.month_days.single()?;
    let (hour, minute) = (cron.hours.single()?, cron.minutes.single()?);
    Some(format!(
        "once on {day} {} {year} at {hour:02}:{minute:02} UTC",
        MONTH_LABELS[month as usize - 1]
    ))
}

fn time_phrase(cron: &Cron) -> String {
    let (minutes, hours) = (&cron.minutes, &cron.hours);
    match (minutes.single(), hours.single()) {
        (Some(m), Some(h)) => return format!("at {h:02}:{m:02}"),
        (Some(m), None) if hours.any => {
            return if m == 0 {
                "every hour".to_string()
            } else {
                format!("every hour at :{m:02}")
            };
        }
        (Some(m), None) => {
            if let [Part::Step { from, to, step }] = hours.parts[..] {
                let span = if from == 0 && to == 23 {
                    String::new()
                } else {
                    format!(" from {from:02}:{m:02} to {to:02}:{m:02}")
                };
                return format!("every {step} hours at :{m:02}{span}");
            }
            if let Some(list) = hours.list() {
                return format!(
                    "at {}",
                    join(
                        &list
                            .iter()
                            .map(|h| format!("{h:02}:{m:02}"))
                            .collect::<Vec<_>>()
                    )
                );
            }
        }
        _ => {}
    }
    if let [Part::Step { from, to, step }] = minutes.parts[..] {
        let every = if step == 1 {
            "every minute".to_string()
        } else {
            format!("every {step} minutes")
        };
        let window = if from == 0 && to == 59 {
            String::new()
        } else {
            format!(" from :{from:02} to :{to:02}")
        };
        let hours_text = if hours.any {
            String::new()
        } else {
            format!(" during hour {}", values_text(hours))
        };
        return format!("{every}{window}{hours_text}");
    }
    if minutes.any && hours.any {
        return "every minute".to_string();
    }
    let minute_text = if minutes.any {
        "every minute".to_string()
    } else {
        format!("at minute {}", values_text(minutes))
    };
    let hour_text = if hours.any {
        "of every hour".to_string()
    } else {
        format!("past hour {}", values_text(hours))
    };
    format!("{minute_text} {hour_text}")
}

/// None when the expression runs every day.
fn day_phrase(cron: &Cron) -> Option<String> {
    if let Some(special) = cron.week_day {
        return Some(match special {
            WeekDay::Last(day) => format!("on the last {} of the month", day_label(day)),
            WeekDay::Nth(day, nth) => format!(
                "on the {} {} of the month",
                ORDINALS[nth as usize - 1],
                day_label(day)
            ),
        });
    }
    if let Some(special) = cron.month_day {
        return Some(match special {
            MonthDay::Last => "on the last day of the month".to_string(),
            MonthDay::LastWeekday => "on the last weekday of the month".to_string(),
            MonthDay::NearestWeekday(day) => format!("on the weekday nearest day {day}"),
        });
    }
    if !cron.week_days.any {
        let days = cron.week_days.values();
        return Some(match days.as_slice() {
            [2, 3, 4, 5, 6] => "every weekday".to_string(),
            [1, 7] => "every weekend".to_string(),
            _ => format!(
                "every {}",
                join(
                    &days
                        .iter()
                        .map(|d| day_label(*d).to_string())
                        .collect::<Vec<_>>()
                )
            ),
        });
    }
    if !cron.month_days.any {
        let in_one_month = cron.months.single().is_some();
        let which = if in_one_month { "" } else { " of every month" };
        return Some(match cron.month_days.list() {
            Some(days) if in_one_month && days.len() == 1 => {
                let every = if cron.years.any { "every year " } else { "" };
                format!(
                    "{every}on {} {}",
                    days[0],
                    MONTH_LABELS[cron.months.single().unwrap_or(1) as usize - 1]
                )
            }
            Some(days) => format!(
                "on day {}{which}",
                join(&days.iter().map(u32::to_string).collect::<Vec<_>>())
            ),
            None => format!("on days {}{which}", values_text(&cron.month_days)),
        });
    }
    None
}

fn month_phrase(cron: &Cron) -> Option<String> {
    let months = &cron.months;
    if months.any {
        return None;
    }
    // "every year on 1 Jan" already names its month.
    let named_by_day =
        months.single().is_some() && cron.month_days.list().is_some_and(|days| days.len() == 1);
    if named_by_day {
        return None;
    }
    Some(join(
        &months
            .values()
            .iter()
            .map(|m| MONTH_LABELS[*m as usize - 1].to_string())
            .collect::<Vec<_>>(),
    ))
}

fn year_phrase(cron: &Cron) -> Option<String> {
    let years = &cron.years;
    if years.any {
        return None;
    }
    Some(match years.parts[..] {
        [Part::Step { from, step, .. }] => format!("every {step} years from {from}"),
        [Part::Value(year)] => format!("in {year} only"),
        _ => format!("in {}", values_text(years)),
    })
}

fn day_label(day: u32) -> &'static str {
    DAY_LABELS[(day as usize).clamp(1, 7) - 1]
}

/// "1, 5-9 and every 3rd from 10", for the generic sentence.
fn values_text(field: &Field) -> String {
    let parts: Vec<String> = field
        .parts
        .iter()
        .map(|part| match *part {
            Part::Value(v) => v.to_string(),
            Part::Range(a, b) => format!("{a}-{b}"),
            Part::Step { from, to, step } => format!("every {step} from {from} to {to}"),
        })
        .collect();
    join(&parts)
}

/// "a", "a and b", "a, b and c".
fn join(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

fn day_and_time(ms: i64) -> String {
    let (year, month, day) = crate::tui::format::civil_from_days(ms.div_euclid(MS_PER_DAY));
    let of_day = ms.rem_euclid(MS_PER_DAY);
    format!(
        "{day} {} {year} at {:02}:{:02} UTC",
        MONTH_LABELS[month as usize - 1],
        of_day / 3_600_000,
        of_day % 3_600_000 / MS_PER_MINUTE
    )
}

// ---------- next runs ----------

/// Up to `count` run times (epoch ms, UTC) strictly after `after_ms` and
/// no later than `until_ms`, searched about two years ahead.
pub fn next_runs(
    expression: &Expression,
    after_ms: i64,
    until_ms: Option<i64>,
    count: usize,
) -> Vec<i64> {
    let in_window = |t: i64| t > after_ms && until_ms.is_none_or(|end| t <= end);
    match expression {
        Expression::At(at) => [*at].into_iter().filter(|t| in_window(*t)).collect(),
        Expression::Rate(..) => Vec::new(),
        Expression::Cron(cron) => cron_runs(cron, after_ms, count, in_window),
    }
}

fn cron_runs(
    cron: &Cron,
    after_ms: i64,
    count: usize,
    in_window: impl Fn(i64) -> bool,
) -> Vec<i64> {
    let hours = cron.hours.values();
    let minutes = cron.minutes.values();
    let first_day = after_ms.div_euclid(MS_PER_DAY);
    let mut runs = Vec::new();
    for day in first_day..first_day + SEARCH_DAYS {
        if !day_matches(cron, day) {
            continue;
        }
        for hour in &hours {
            for minute in &minutes {
                let t = day * MS_PER_DAY
                    + i64::from(*hour) * 3_600_000
                    + i64::from(*minute) * MS_PER_MINUTE;
                if t <= after_ms {
                    continue;
                }
                if !in_window(t) {
                    return runs;
                }
                runs.push(t);
                if runs.len() == count {
                    return runs;
                }
            }
        }
    }
    runs
}

fn day_matches(cron: &Cron, day: i64) -> bool {
    use crate::tui::format::civil_from_days;
    let (year, month, dom) = civil_from_days(day);
    let year = u32::try_from(year).unwrap_or(0);
    // 1970-01-01 was a Thursday (5 in AWS numbering, Sunday = 1).
    let dow = ((day + 4).rem_euclid(7) + 1) as u32;
    let last_dom = days_in_month(i64::from(year), month);
    let month_day_ok = match cron.month_day {
        Some(MonthDay::Last) => dom == last_dom,
        Some(MonthDay::LastWeekday) => dom == last_weekday(dom, last_dom, dow),
        Some(MonthDay::NearestWeekday(target)) => {
            dom == nearest_weekday(dom, target.min(last_dom), last_dom, dow)
        }
        None => cron.month_days.matches(dom),
    };
    let week_day_ok = match cron.week_day {
        Some(WeekDay::Last(target)) => dow == target && dom + 7 > last_dom,
        Some(WeekDay::Nth(target, nth)) => dow == target && (dom - 1) / 7 + 1 == nth,
        None => cron.week_days.matches(dow),
    };
    cron.years.matches(year) && cron.months.matches(month) && month_day_ok && week_day_ok
}

fn days_in_month(year: i64, month: u32) -> u32 {
    use crate::tui::format::days_from_civil;
    let (next_year, next_month) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    (days_from_civil(next_year, next_month, 1) - days_from_civil(year, month, 1)) as u32
}

/// The weekday (Mon-Fri) of `day`'s month nearest to `target`, staying in the month.
fn nearest_weekday(dom: u32, target: u32, last_dom: u32, dow: u32) -> u32 {
    match weekday_of(dow, dom, target) {
        7 if target == 1 => target + 2,
        7 => target - 1,
        1 if target == last_dom => target - 2,
        1 => target + 1,
        _ => target,
    }
}

/// The last Mon-Fri of the month.
fn last_weekday(dom: u32, last_dom: u32, dow: u32) -> u32 {
    match weekday_of(dow, dom, last_dom) {
        7 => last_dom - 1,
        1 => last_dom - 2,
        _ => last_dom,
    }
}

/// The weekday (1 = Sunday) of day-of-month `other`, given `dom`'s weekday.
fn weekday_of(dow: u32, dom: u32, other: u32) -> u32 {
    let shift = i64::from(other) - i64::from(dom);
    ((i64::from(dow) - 1 + shift).rem_euclid(7) + 1) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn describe_text(text: &str) -> Result<String, CronError> {
        parse(text).map(|e| describe(&e))
    }

    fn says(text: &str) -> String {
        describe_text(text).unwrap_or_else(|e| panic!("{text}: {e}"))
    }

    /// 2026-11-08T12:00Z, a Sunday.
    const NOW: i64 = 1_794_139_200_000;

    fn at(text: &str) -> i64 {
        crate::tui::format::epoch_ms(&serde_json::Value::String(text.into())).unwrap()
    }

    #[test]
    fn describes_what_the_web_generates() {
        assert_eq!(
            says("cron(0 9 ? * MON-FRI *)"),
            "every weekday at 09:00 UTC"
        );
        assert_eq!(says("cron(0 2 * * ? *)"), "every day at 02:00 UTC");
        assert_eq!(
            says("cron(30 9 ? * Mon,Wed *)"),
            "every Monday and Wednesday at 09:30 UTC"
        );
        assert_eq!(
            says("cron(15 6 1 * ? *)"),
            "on day 1 of every month at 06:15 UTC"
        );
        assert_eq!(
            says("cron(15 6 1,15 * ? *)"),
            "on day 1 and 15 of every month at 06:15 UTC"
        );
        assert_eq!(
            says("cron(0 8 ? * Mon#1 *)"),
            "on the first Monday of the month at 08:00 UTC"
        );
        assert_eq!(
            says("cron(0 8 1 1 ? 2026/2)"),
            "on 1 Jan at 08:00 UTC, every 2 years from 2026"
        );
        assert_eq!(
            says("cron(10 7 26 5 ? 2026)"),
            "once on 26 May 2026 at 07:10 UTC"
        );
    }

    #[test]
    fn minute_steps_say_where_they_start() {
        assert_eq!(says("cron(0/15 * ? * * *)"), "every 15 minutes UTC");
        // AWS reads 14/1 as minutes 14-59 of every hour, not every minute.
        assert_eq!(
            says("cron(14/1 * ? * * *)"),
            "every minute from :14 to :59 UTC"
        );
        assert_eq!(
            says("cron(04/4 * ? * * *)"),
            "every 4 minutes from :04 to :59 UTC"
        );
        assert_eq!(says("cron(30 0/6 ? * * *)"), "every 6 hours at :30 UTC");
        assert_eq!(says("cron(5 * * * ? *)"), "every hour at :05 UTC");
    }

    #[test]
    fn sunday_is_one_and_saturday_seven() {
        assert_eq!(says("cron(0 10 ? * 1 *)"), "every Sunday at 10:00 UTC");
        assert_eq!(says("cron(0 10 ? * 7 *)"), "every Saturday at 10:00 UTC");
        assert_eq!(
            says("cron(0 10 ? * SAT,SUN *)"),
            "every weekend at 10:00 UTC"
        );
        assert_eq!(
            says("cron(0 10 ? * 6L *)"),
            "on the last Friday of the month at 10:00 UTC"
        );
    }

    #[test]
    fn reads_rates_and_one_off_times() {
        assert_eq!(says("rate(5 minutes)"), "every 5 minutes");
        assert_eq!(says("rate(1 hour)"), "every hour");
        assert_eq!(
            says("at(2026-05-26T07:10:00)"),
            "once on 26 May 2026 at 07:10 UTC"
        );
    }

    #[test]
    fn explains_bad_expressions() {
        let err = |t: &str| describe_text(t).unwrap_err().to_string();
        assert!(err("cron(0 9 * *)").contains("6 fields"));
        assert!(err("cron(0 9 * * MON *)").contains("?"));
        assert!(err("cron(61 9 ? * * *)").contains("minute 61 is outside 0-59"));
        assert!(err("cron(0 9 ? * 0 *)").contains("weekday 0 is outside 1-7"));
        assert!(err("cron(0 9 ? * FUNDAY *)").contains("not a valid weekday"));
    }

    #[test]
    fn next_weekday_runs_skip_the_weekend() {
        let cron = parse("cron(0 9 ? * MON-FRI *)").unwrap();
        let runs = next_runs(&cron, NOW, None, 3);
        assert_eq!(
            runs,
            vec![
                at("2026-11-09T09:00:00Z"),
                at("2026-11-10T09:00:00Z"),
                at("2026-11-11T09:00:00Z")
            ]
        );
    }

    #[test]
    fn next_runs_honour_steps_special_days_and_end_dates() {
        let every_15 = parse("cron(0/15 * ? * * *)").unwrap();
        assert_eq!(
            next_runs(&every_15, NOW, None, 2),
            vec![NOW + 15 * 60_000, NOW + 30 * 60_000]
        );
        let last_friday = parse("cron(0 10 ? * 6L *)").unwrap();
        assert_eq!(
            next_runs(&last_friday, NOW, None, 1),
            vec![at("2026-11-27T10:00:00Z")]
        );
        let first_monday = parse("cron(0 8 ? * 2#1 *)").unwrap();
        assert_eq!(
            next_runs(&first_monday, NOW, None, 1),
            vec![at("2026-12-07T08:00:00Z")]
        );
        let last_day = parse("cron(0 0 L * ? *)").unwrap();
        assert_eq!(
            next_runs(&last_day, NOW, None, 1),
            vec![at("2026-11-30T00:00:00Z")]
        );
        let nearest = parse("cron(0 0 15W * ? *)").unwrap();
        // 15 Nov 2026 is a Sunday, so the nearest weekday is Monday the 16th.
        assert_eq!(
            next_runs(&nearest, NOW, None, 1),
            vec![at("2026-11-16T00:00:00Z")]
        );
        let ended = next_runs(&every_15, NOW, Some(NOW + 20 * 60_000), 5);
        assert_eq!(ended, vec![NOW + 15 * 60_000]);
        let past_once = parse("cron(10 7 26 5 ? 2026)").unwrap();
        assert!(next_runs(&past_once, NOW, None, 1).is_empty());
    }
}
