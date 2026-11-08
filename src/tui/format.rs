//! Turns raw API values into the short, readable text a table cell or a
//! detail line shows: relative times, sizes, durations, money, counts and
//! status badges. Pure functions; "now" is passed in so tests are stable.

use serde_json::Value;

use crate::tui::theme::BadgeTone;

/// What missing, null or empty values show as.
pub const MISSING: &str = "—";

const MS_PER_SECOND: i64 = 1_000;
const MS_PER_MINUTE: i64 = 60 * MS_PER_SECOND;
const MS_PER_HOUR: i64 = 60 * MS_PER_MINUTE;
const MS_PER_DAY: i64 = 24 * MS_PER_HOUR;
/// Numbers above this are epoch milliseconds, below it epoch seconds
/// (1e11 seconds is the year 5138; 1e11 ms is 1973).
const EPOCH_MS_THRESHOLD: f64 = 1e11;

/// How a value is shown.
#[derive(Debug, Clone, Copy)]
#[allow(
    dead_code,
    reason = "spec vocabulary for the screens being converted next"
)]
pub enum Format {
    Text,
    /// "3h ago" / "in 2d"; parses ISO-8601, epoch seconds and epoch ms.
    RelTime,
    /// "2026-10-14 04:32 UTC".
    DateTime,
    /// "2026-10-14".
    Date,
    /// "1.4 MB"; accepts numbers and strings like "4MB".
    Bytes,
    /// "850 ms", "12.3 s", "4m 05s", "2h 03m".
    Duration(TimeUnit),
    /// "$1.24".
    Usd,
    /// "17,503,151"; decimals kept to two places.
    Number,
    /// Length of an array (or the number itself).
    Count,
    /// "yes" / "no".
    Bool,
    /// A badge from a raw status value.
    Status(&'static StatusMap),
    /// A code mapped to a display label ("AWSS3" → "AWS S3"); unknown codes
    /// show as they are.
    Label(&'static [(&'static str, &'static str)]),
    /// A rate already on a 0–100 scale ("98.6%"), as the agent builder and
    /// ETL send `successRate`.
    Percent,
    /// Large counts shortened: "4.4M", "12.3k", "950".
    Compact,
    /// Integer cents as dollars (`usedCents: 800` → "$8.00").
    Cents,
    /// A version number as "v10".
    Version,
    /// A provider model id as a readable name
    /// (`in.openai.gpt-5.6-luna` → "GPT-5.6 Luna"); unknown ids lose only
    /// their region and vendor prefixes.
    Model,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(
    dead_code,
    reason = "spec vocabulary for the screens being converted next"
)]
pub enum TimeUnit {
    Millis,
    Seconds,
}

/// Raw status values mapped to a label and a tone. Matching ignores case;
/// unknown values keep their text and take `fallback`.
#[derive(Debug)]
pub struct StatusMap {
    pub entries: &'static [(&'static str, &'static str, BadgeTone)],
    pub fallback: BadgeTone,
}

impl StatusMap {
    pub fn badge(&self, raw: &str) -> (String, BadgeTone) {
        self.entries
            .iter()
            .find(|(value, _, _)| value.eq_ignore_ascii_case(raw))
            .map(|(_, label, tone)| (label.to_string(), *tone))
            .unwrap_or_else(|| (raw.to_string(), self.fallback))
    }
}

/// Text plus an optional badge tone for the renderer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shown {
    pub text: String,
    pub tone: Option<BadgeTone>,
}

impl Shown {
    fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            tone: None,
        }
    }

    fn missing() -> Self {
        Self {
            text: MISSING.to_string(),
            tone: Some(BadgeTone::Muted),
        }
    }
}

/// Formats `value` (None = the field is absent) for display.
pub fn show(value: Option<&Value>, format: Format, now_ms: i64) -> Shown {
    let Some(value) = value.filter(|v| !is_blank(v)) else {
        return Shown::missing();
    };
    let text = match format {
        Format::Text => Some(text_of(value)),
        Format::RelTime => epoch_ms(value).map(|t| relative_time(t, now_ms)),
        Format::DateTime => epoch_ms(value).map(date_time),
        Format::Date => epoch_ms(value).map(|t| date_time(t)[..10].to_string()),
        Format::Bytes => byte_count(value).map(bytes),
        Format::Duration(unit) => number(value).map(|n| duration(to_ms(n, unit))),
        Format::Usd => number(value).map(usd),
        Format::Number => number(value).map(grouped_number),
        Format::Count => count(value).map(|n| group_digits(&n.to_string())),
        Format::Bool => bool_of(value).map(|b| if b { "yes" } else { "no" }.to_string()),
        Format::Status(map) => {
            let (label, tone) = map.badge(&text_of(value));
            return Shown {
                text: label,
                tone: Some(tone),
            };
        }
        Format::Label(labels) => {
            let raw = text_of(value);
            let label = labels
                .iter()
                .find(|(code, _)| code.eq_ignore_ascii_case(&raw))
                .map_or(raw, |(_, label)| label.to_string());
            Some(label)
        }
        Format::Percent => number(value).map(percent),
        Format::Compact => number(value).map(compact),
        Format::Cents => number(value).map(|cents| usd(cents / 100.0)),
        Format::Version => Some(format!("v{}", text_of(value))),
        Format::Model => Some(model_name(&text_of(value))),
    };
    // A value that doesn't parse as the expected kind still shows as text,
    // so a schema surprise is visible instead of hidden.
    Shown::plain(text.unwrap_or_else(|| text_of(value)))
}

fn is_blank(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(s) => s.trim().is_empty(),
        _ => false,
    }
}

/// Scalars as plain text; arrays of scalars joined; objects compact JSON.
pub fn text_of(value: &Value) -> String {
    match value {
        Value::Null => MISSING.to_string(),
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Array(items) if items.iter().all(|v| !v.is_object() && !v.is_array()) => {
            items.iter().map(text_of).collect::<Vec<_>>().join(", ")
        }
        Value::Array(items) => format!("{} item(s)", items.len()),
        Value::Object(_) => value.to_string(),
    }
}

fn number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn count(value: &Value) -> Option<u64> {
    match value {
        Value::Array(items) => Some(items.len() as u64),
        Value::Object(map) => Some(map.len() as u64),
        other => number(other).map(|n| n.max(0.0) as u64),
    }
}

fn bool_of(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(b) => Some(*b),
        Value::String(s) => match s.to_ascii_lowercase().as_str() {
            "true" | "yes" | "1" => Some(true),
            "false" | "no" | "0" => Some(false),
            _ => None,
        },
        Value::Number(n) => n.as_f64().map(|n| n != 0.0),
        _ => None,
    }
}

fn to_ms(n: f64, unit: TimeUnit) -> f64 {
    match unit {
        TimeUnit::Millis => n,
        TimeUnit::Seconds => n * MS_PER_SECOND as f64,
    }
}

/// Cuts `text` to `width` characters with a trailing ellipsis; newlines and
/// tabs become spaces so a cell stays on one line.
pub fn truncate(text: &str, width: usize) -> String {
    let flat = text.replace(['\n', '\r', '\t'], " ");
    if flat.chars().count() <= width {
        return flat;
    }
    let kept: String = flat.chars().take(width.saturating_sub(1)).collect();
    format!("{kept}…")
}

/// Word-wraps `text` to `width` characters, keeping its line breaks; a word
/// longer than the width is split.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    for paragraph in text.lines() {
        let mut line = String::new();
        for word in paragraph.split_whitespace() {
            let mut word = word.to_string();
            while word.chars().count() > width {
                if !line.is_empty() {
                    out.push(std::mem::take(&mut line));
                }
                let head: String = word.chars().take(width).collect();
                word = word.chars().skip(width).collect();
                out.push(head);
            }
            let needed =
                line.chars().count() + usize::from(!line.is_empty()) + word.chars().count();
            if needed > width && !line.is_empty() {
                out.push(std::mem::take(&mut line));
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(&word);
        }
        out.push(line);
    }
    out
}

// ---------- numbers ----------

/// "17503151" → "17,503,151". Leaves non-digit text alone.
pub fn group_digits(number: &str) -> String {
    let (sign, rest) = match number.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", number),
    };
    let (digits, fraction) = match rest.split_once('.') {
        Some((d, f)) => (d, Some(f)),
        None => (rest, None),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return number.to_string();
    }
    let mut out = String::new();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    match fraction {
        Some(f) => format!("{sign}{out}.{f}"),
        None => format!("{sign}{out}"),
    }
}

fn grouped_number(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        return group_digits(&format!("{n:.0}"));
    }
    let fixed = format!("{n:.2}");
    let trimmed = fixed.trim_end_matches('0').trim_end_matches('.');
    group_digits(trimmed)
}

fn usd(n: f64) -> String {
    let sign = if n < 0.0 { "-" } else { "" };
    let abs = n.abs();
    // Sub-cent model costs are common; two decimals would show "$0.00".
    let amount = if abs > 0.0 && abs < 0.01 {
        format!("{abs:.4}")
    } else {
        group_digits(&format!("{abs:.2}"))
    };
    format!("{sign}${amount}")
}

fn percent(n: f64) -> String {
    let fixed = format!("{n:.1}");
    format!("{}%", fixed.trim_end_matches(".0"))
}

/// "4.4M", "12.3k", "950"; one decimal below 100 of a unit.
pub fn compact(n: f64) -> String {
    const UNITS: [(f64, &str); 3] = [(1e9, "B"), (1e6, "M"), (1e3, "k")];
    let abs = n.abs();
    for (size, suffix) in UNITS {
        if abs >= size {
            let scaled = n / size;
            let text = if scaled.abs() < 100.0 {
                format!("{scaled:.1}")
            } else {
                format!("{scaled:.0}")
            };
            return format!("{}{suffix}", text.trim_end_matches(".0"));
        }
    }
    grouped_number(n)
}

/// Bedrock inference-profile prefixes (`global.`, `in.`) and vendor
/// prefixes the web strips before naming a model (`utils/modelDisplayName.js`).
const MODEL_REGIONS: &[&str] = &["global", "in", "us", "eu", "apac", "jp", "au"];
const MODEL_VENDORS: &[&str] = &[
    "anthropic",
    "openai",
    "xai",
    "moonshotai",
    "amazon",
    "cohere",
    "meta",
    "mistral",
];

/// A model family's id pattern and how to name a match.
type ModelRule = (regex::Regex, fn(&regex::Captures) -> String);

static MODEL_RULES: std::sync::LazyLock<[ModelRule; 4]> = std::sync::LazyLock::new(|| {
    let rule = |pattern: &str| regex::Regex::new(pattern).expect("model name patterns are valid");
    [
        (rule(r"^claude-([a-z]+)-(\d+)(?:-(\d))?(?:$|[^\d])"), |c| {
            let minor = c
                .get(3)
                .map_or(String::new(), |m| format!(".{}", m.as_str()));
            format!("Claude {} {}{minor}", capitalize(&c[1]), &c[2])
        }),
        (rule(r"^gpt-(\d+(?:\.\d+)?)(?:-([a-z]+))?$"), |c| {
            let tier = c
                .get(2)
                .map_or(String::new(), |m| format!(" {}", capitalize(m.as_str())));
            format!("GPT-{}{tier}", &c[1])
        }),
        (rule(r"^grok-(\d+(?:\.\d+)?)$"), |c| {
            format!("Grok {}", &c[1])
        }),
        (rule(r"^kimi-k(\d+(?:\.\d+)?)$"), |c| {
            format!("Kimi K{}", &c[1])
        }),
    ]
});

fn capitalize(word: &str) -> String {
    let mut chars = word.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

/// The model id without its ARN path, region and vendor prefixes.
fn bare_model_id(id: &str) -> &str {
    let mut bare = id.rsplit('/').next().unwrap_or(id);
    for prefixes in [MODEL_REGIONS, MODEL_VENDORS] {
        if let Some((head, rest)) = bare.split_once('.') {
            if prefixes.contains(&head) {
                bare = rest;
            }
        }
    }
    bare
}

/// A readable model name: a known family's name, else the bare id.
pub fn model_name(id: &str) -> String {
    let bare = bare_model_id(id.trim());
    let lower = bare.to_ascii_lowercase();
    MODEL_RULES
        .iter()
        .find_map(|(pattern, name)| pattern.captures(&lower).map(|c| name(&c)))
        .unwrap_or_else(|| bare.to_string())
}

const BYTE_UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];

/// A size as bytes: numbers, numeric text, or text such as `"4MB"`.
pub(crate) fn byte_count(value: &Value) -> Option<f64> {
    if let Some(n) = number(value) {
        return Some(n);
    }
    // Some providers send sizes as text: "4MB", "1.5 GB".
    let text = value.as_str()?.trim().to_ascii_uppercase();
    let split = text
        .find(|c: char| c.is_ascii_alphabetic())
        .unwrap_or(text.len());
    let (digits, unit) = text.split_at(split);
    let amount: f64 = digits.trim().parse().ok()?;
    let unit = unit.trim().trim_end_matches("IB").trim_end_matches('B');
    let power = match unit {
        "" => 0,
        "K" => 1,
        "M" => 2,
        "G" => 3,
        "T" => 4,
        "P" => 5,
        _ => return None,
    };
    Some(amount * 1024f64.powi(power))
}

fn bytes(n: f64) -> String {
    let mut size = n.max(0.0);
    let mut unit = 0;
    while size >= 1024.0 && unit < BYTE_UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{size:.0} B")
    } else if size < 10.0 {
        format!("{size:.1} {}", BYTE_UNITS[unit])
    } else {
        format!("{size:.0} {}", BYTE_UNITS[unit])
    }
}

fn duration(ms: f64) -> String {
    let ms = ms.max(0.0);
    if ms < MS_PER_SECOND as f64 {
        return format!("{ms:.0} ms");
    }
    let total_ms = ms as i64;
    if total_ms < MS_PER_MINUTE {
        return format!("{:.1} s", ms / MS_PER_SECOND as f64);
    }
    let secs = total_ms / MS_PER_SECOND;
    let (days, hours, minutes, seconds) =
        (secs / 86_400, secs / 3_600 % 24, secs / 60 % 60, secs % 60);
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes:02}m")
    } else {
        format!("{minutes}m {seconds:02}s")
    }
}

// ---------- time ----------

/// Milliseconds since the epoch for numbers (seconds or ms) and ISO-8601
/// text (with `Z`, an offset, or no zone, which is read as UTC).
pub fn epoch_ms(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => n.as_f64().map(epoch_number_ms),
        Value::String(s) => {
            let s = s.trim();
            if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) {
                return s.parse::<f64>().ok().map(epoch_number_ms);
            }
            parse_iso8601(s)
        }
        _ => None,
    }
}

fn epoch_number_ms(n: f64) -> i64 {
    if n.abs() >= EPOCH_MS_THRESHOLD {
        n as i64
    } else {
        (n * MS_PER_SECOND as f64) as i64
    }
}

/// `YYYY-MM-DD[(T| )HH:MM[:SS[.fff]]][Z|±HH[:]MM]`.
fn parse_iso8601(text: &str) -> Option<i64> {
    let date = text.get(..10)?;
    let mut parts = date.split('-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: u32 = parts.next()?.parse().ok()?;
    let day: u32 = parts.next()?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let days = days_from_civil(year, month, day);
    let rest = &text[10..];
    if rest.is_empty() {
        return Some(days * MS_PER_DAY);
    }
    let rest = rest.strip_prefix(['T', 't', ' '])?;
    let zone_at = rest
        .char_indices()
        .skip(1)
        .find(|(_, c)| matches!(c, 'Z' | 'z' | '+' | '-'))
        .map_or(rest.len(), |(i, _)| i);
    let (clock, zone) = rest.split_at(zone_at);
    let time_ms = parse_clock(clock)?;
    let offset_ms = parse_zone(zone)?;
    Some(days * MS_PER_DAY + time_ms - offset_ms)
}

fn parse_clock(clock: &str) -> Option<i64> {
    let mut fields = clock.split(':');
    let hours: i64 = fields.next()?.parse().ok()?;
    let minutes: i64 = fields.next()?.parse().ok()?;
    let seconds: f64 = fields.next().map_or(Some(0.0), |s| s.parse().ok())?;
    Some(hours * MS_PER_HOUR + minutes * MS_PER_MINUTE + (seconds * 1000.0).round() as i64)
}

fn parse_zone(zone: &str) -> Option<i64> {
    if zone.is_empty() || zone.eq_ignore_ascii_case("z") {
        return Some(0);
    }
    let sign = if zone.starts_with('-') { -1 } else { 1 };
    let digits: String = zone[1..].chars().filter(|c| *c != ':').collect();
    if digits.len() != 4 {
        return None;
    }
    let hours: i64 = digits[..2].parse().ok()?;
    let minutes: i64 = digits[2..].parse().ok()?;
    Some(sign * (hours * MS_PER_HOUR + minutes * MS_PER_MINUTE))
}

/// Days since 1970-01-01 (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(month);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// "2026-10-14 04:32 UTC". The TUI has no time-zone database, so UTC it is.
pub fn date_time(epoch_ms: i64) -> String {
    let days = epoch_ms.div_euclid(MS_PER_DAY);
    let of_day = epoch_ms.rem_euclid(MS_PER_DAY);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02} UTC",
        of_day / MS_PER_HOUR,
        of_day % MS_PER_HOUR / MS_PER_MINUTE
    )
}

/// "2026-10-14T04:32:00Z", for query parameters that take a UTC instant.
pub fn iso8601_utc(epoch_ms: i64) -> String {
    let days = epoch_ms.div_euclid(MS_PER_DAY);
    let of_day = epoch_ms.rem_euclid(MS_PER_DAY);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        of_day / MS_PER_HOUR,
        of_day % MS_PER_HOUR / MS_PER_MINUTE,
        of_day % MS_PER_MINUTE / MS_PER_SECOND
    )
}

/// "just now", "5m ago", "3h ago", "2d ago", "4mo ago", "1y ago", "in 2h".
pub fn relative_time(epoch_ms: i64, now_ms: i64) -> String {
    let delta = now_ms - epoch_ms;
    let span = delta.abs();
    let amount = if span < 45 * MS_PER_SECOND {
        return "just now".to_string();
    } else if span < MS_PER_HOUR {
        format!("{}m", (span / MS_PER_MINUTE).max(1))
    } else if span < MS_PER_DAY {
        format!("{}h", span / MS_PER_HOUR)
    } else if span < 30 * MS_PER_DAY {
        format!("{}d", span / MS_PER_DAY)
    } else if span < 365 * MS_PER_DAY {
        format!("{}mo", span / (30 * MS_PER_DAY))
    } else {
        format!("{}y", span / (365 * MS_PER_DAY))
    };
    if delta >= 0 {
        format!("{amount} ago")
    } else {
        format!("in {amount}")
    }
}

/// Config keys whose values must never be shown, whatever the server sent
/// (the web app's rule in `configuredDetails.js`).
static SECRET_KEY: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(
        r"(?i)(password|secret|token|credential|private[_-]?key|access[_-]?key|api[_-]?key|account[_-]?key|client[_-]?secret|security[_-]?token|headers)",
    )
    .expect("the secret-key pattern is a valid regex")
});

/// What masked values show as.
pub const MASKED: &str = "••••••";

pub fn is_secret_key(key: &str) -> bool {
    SECRET_KEY.is_match(key)
}

/// "bucketName" → "Bucket name", "SASL_mechanism" → "SASL mechanism".
pub fn humanize_key(key: &str) -> String {
    let mut words: Vec<String> = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = key.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        let boundary = c.is_uppercase()
            && i > 0
            && (chars[i - 1].is_lowercase() || chars.get(i + 1).is_some_and(|n| n.is_lowercase()))
            && !current.is_empty();
        if c == '_' || c == '-' || c == ' ' || boundary {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            if !c.is_alphanumeric() {
                continue;
            }
        }
        current.push(c);
    }
    if !current.is_empty() {
        words.push(current);
    }
    let words: Vec<String> = words
        .into_iter()
        .enumerate()
        .map(|(i, word)| {
            let is_acronym =
                word.len() > 1 && word.chars().all(|c| c.is_uppercase() || c.is_ascii_digit());
            match (i, is_acronym) {
                (_, true) => word,
                (0, false) => {
                    let mut chars = word.chars();
                    chars
                        .next()
                        .map(|first| {
                            first
                                .to_uppercase()
                                .chain(chars.map(|c| c.to_ascii_lowercase()))
                                .collect()
                        })
                        .unwrap_or_default()
                }
                _ => word.to_lowercase(),
            }
        })
        .collect();
    words.join(" ")
}

/// Wall-clock milliseconds since the epoch.
pub fn system_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 2026-11-08T12:00:00Z.
    const NOW: i64 = 1_794_139_200_000;

    fn text(value: Value, format: Format) -> String {
        show(Some(&value), format, NOW).text
    }

    #[test]
    fn parses_every_time_shape_providers_send() {
        let iso_z = epoch_ms(&json!("2026-11-08T12:00:00Z")).unwrap();
        assert_eq!(iso_z, NOW);
        assert_eq!(epoch_ms(&json!("2026-11-08T12:00:00.000Z")), Some(NOW));
        assert_eq!(epoch_ms(&json!("2026-11-08T14:00:00+02:00")), Some(NOW));
        assert_eq!(
            epoch_ms(&json!("2026-11-08T12:00:00.419868+00:00")),
            Some(NOW + 420)
        );
        assert_eq!(epoch_ms(&json!("2026-11-08 12:00:00")), Some(NOW));
        assert_eq!(epoch_ms(&json!("2026-11-08T12:00:00")), Some(NOW));
        assert_eq!(epoch_ms(&json!(NOW)), Some(NOW));
        assert_eq!(epoch_ms(&json!(NOW / 1000)), Some(NOW));
        assert_eq!(epoch_ms(&json!("1794139200")), Some(NOW));
        assert_eq!(epoch_ms(&json!("not a date")), None);
    }

    #[test]
    fn relative_times_read_naturally() {
        assert_eq!(relative_time(NOW - 10_000, NOW), "just now");
        assert_eq!(relative_time(NOW - 5 * MS_PER_MINUTE, NOW), "5m ago");
        assert_eq!(relative_time(NOW - 3 * MS_PER_HOUR, NOW), "3h ago");
        assert_eq!(relative_time(NOW - 2 * MS_PER_DAY, NOW), "2d ago");
        assert_eq!(relative_time(NOW - 90 * MS_PER_DAY, NOW), "3mo ago");
        assert_eq!(relative_time(NOW - 800 * MS_PER_DAY, NOW), "2y ago");
        assert_eq!(relative_time(NOW + 2 * MS_PER_HOUR, NOW), "in 2h");
        assert_eq!(
            text(json!("2026-11-08T09:00:00Z"), Format::RelTime),
            "3h ago"
        );
    }

    #[test]
    fn absolute_dates_are_utc() {
        assert_eq!(date_time(NOW), "2026-11-08 12:00 UTC");
        assert_eq!(date_time(0), "1970-01-01 00:00 UTC");
        assert_eq!(iso8601_utc(NOW - 1_500), "2026-11-08T11:59:58Z");
        assert_eq!(epoch_ms(&json!(iso8601_utc(NOW))), Some(NOW));
        assert_eq!(
            text(json!("2024-02-29T23:59:00Z"), Format::Date),
            "2024-02-29"
        );
    }

    #[test]
    fn sizes_durations_money_and_numbers() {
        assert_eq!(text(json!(703_577), Format::Bytes), "687 KB");
        assert_eq!(text(json!(512), Format::Bytes), "512 B");
        assert_eq!(text(json!("4MB"), Format::Bytes), "4.0 MB");
        assert_eq!(
            text(json!(850), Format::Duration(TimeUnit::Millis)),
            "850 ms"
        );
        assert_eq!(
            text(json!(12_300), Format::Duration(TimeUnit::Millis)),
            "12.3 s"
        );
        assert_eq!(
            text(json!(245), Format::Duration(TimeUnit::Seconds)),
            "4m 05s"
        );
        assert_eq!(
            text(json!(7_380), Format::Duration(TimeUnit::Seconds)),
            "2h 03m"
        );
        assert_eq!(text(json!("1.244280580800000000"), Format::Usd), "$1.24");
        assert_eq!(text(json!(0.0034), Format::Usd), "$0.0034");
        assert_eq!(text(json!(12_345.5), Format::Usd), "$12,345.50");
        assert_eq!(text(json!(17_503_151), Format::Number), "17,503,151");
        assert_eq!(text(json!(1234.5678), Format::Number), "1,234.57");
        assert_eq!(text(json!([1, 2, 3]), Format::Count), "3");
        assert_eq!(text(json!(true), Format::Bool), "yes");
    }

    #[test]
    fn missing_values_show_a_dash() {
        assert_eq!(show(None, Format::Text, NOW).text, MISSING);
        assert_eq!(text(Value::Null, Format::RelTime), MISSING);
        assert_eq!(text(json!("  "), Format::Text), MISSING);
    }

    #[test]
    fn unparseable_values_fall_back_to_their_text() {
        assert_eq!(text(json!("soon"), Format::RelTime), "soon");
    }

    static RUN_STATUS: StatusMap = StatusMap {
        entries: &[
            ("SUCCEEDED", "Succeeded", BadgeTone::Good),
            ("FAILED", "Failed", BadgeTone::Bad),
        ],
        fallback: BadgeTone::Info,
    };

    #[test]
    fn statuses_become_badges_with_tones() {
        let shown = show(Some(&json!("failed")), Format::Status(&RUN_STATUS), NOW);
        assert_eq!(shown.text, "Failed");
        assert_eq!(shown.tone, Some(BadgeTone::Bad));
        let unknown = show(Some(&json!("PAUSED")), Format::Status(&RUN_STATUS), NOW);
        assert_eq!(
            (unknown.text.as_str(), unknown.tone),
            ("PAUSED", Some(BadgeTone::Info))
        );
    }

    #[test]
    fn labels_map_codes_and_keep_unknown_ones() {
        const TYPES: &[(&str, &str)] = &[("AWSS3", "AWS S3")];
        assert_eq!(text(json!("AWSS3"), Format::Label(TYPES)), "AWS S3");
        assert_eq!(text(json!("KAFKA"), Format::Label(TYPES)), "KAFKA");
    }

    #[test]
    fn rates_counts_cents_and_versions() {
        assert_eq!(text(json!(98.62), Format::Percent), "98.6%");
        assert_eq!(text(json!(100), Format::Percent), "100%");
        assert_eq!(text(json!(319_705_041), Format::Compact), "320M");
        assert_eq!(text(json!(4_351_577), Format::Compact), "4.4M");
        assert_eq!(text(json!(12_300), Format::Compact), "12.3k");
        assert_eq!(text(json!(950), Format::Compact), "950");
        assert_eq!(text(json!(800), Format::Cents), "$8.00");
        assert_eq!(text(json!(10), Format::Version), "v10");
    }

    #[test]
    fn model_ids_read_like_the_web_names_them() {
        assert_eq!(model_name("in.openai.gpt-5.6-luna"), "GPT-5.6 Luna");
        assert_eq!(model_name("global.openai.gpt-6-luna"), "GPT-6 Luna");
        assert_eq!(
            model_name("global.anthropic.claude-sonnet-5-5"),
            "Claude Sonnet 5.5"
        );
        assert_eq!(model_name("xai.grok-4.7"), "Grok 4.7");
        assert_eq!(model_name("moonshotai.kimi-k3.1"), "Kimi K3.1");
        // Unknown families keep the id, minus the region and vendor.
        assert_eq!(model_name("cohere.rerank-v3-5:0"), "rerank-v3-5:0");
        assert_eq!(text(json!("us.meta.llama-5"), Format::Model), "llama-5");
    }

    #[test]
    fn truncates_with_an_ellipsis() {
        assert_eq!(truncate("abc", 5), "abc");
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("a\nb", 5), "a b");
    }

    #[test]
    fn wrapping_keeps_line_breaks_and_splits_long_words() {
        assert_eq!(
            wrap("ab cd ef\n\nxyzxyzxyz", 5),
            vec!["ab cd", "ef", "", "xyzxy", "zxyz"]
        );
    }

    #[test]
    fn humanizes_config_keys() {
        assert_eq!(humanize_key("bucketName"), "Bucket name");
        assert_eq!(humanize_key("SASL_mechanism"), "SASL mechanism");
        assert_eq!(humanize_key("lakeHouseUrl"), "Lake house url");
        assert_eq!(humanize_key("region"), "Region");
    }

    #[test]
    fn spots_secret_keys() {
        for key in [
            "password",
            "secretAccessKey",
            "api_key",
            "ClientSecret",
            "headers",
            "sessionToken",
        ] {
            assert!(is_secret_key(key), "{key}");
        }
        for key in ["bucketName", "region", "userName"] {
            assert!(!is_secret_key(key), "{key}");
        }
    }

    #[test]
    fn groups_digits_keeping_fractions() {
        assert_eq!(group_digits("1234"), "1,234");
        assert_eq!(group_digits("-1234567.5"), "-1,234,567.5");
        assert_eq!(group_digits("abc"), "abc");
    }
}
