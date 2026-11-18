//! The pure parts of a watch: its cron, its name, what it records about
//! itself, and the failure email.
//!
//! Crons are EventBridge Scheduler expressions in UTC (the Schedule
//! Lambda's `CreateScheduleCommand`, Sunday = 1), which the Schedule API
//! validates as at least 17 characters (`schedule.schema.mjs`).

use anyhow::{bail, Result};
use serde_json::{json, Value};

use crate::cmd::lakehouse_table::LakehouseTable;
use crate::cmd::run_wait::parse_timeout;

/// Every watch's schedule and job start with this, so `list` finds them.
pub const NAME_PREFIX: &str = "gz-quality-watch-";
/// The job config field holding a watch's own record.
pub const METADATA_FIELD: &str = "gzQualityWatch";
const METADATA_VERSION: u32 = 1;
/// EventBridge Scheduler names hold at most 64 characters, and the Schedule
/// API passes the name through as is.
const MAX_NAME_CHARS: usize = 64;
const MINUTE_STEPS: [u64; 5] = [5, 10, 15, 20, 30];
const HOUR_STEPS: [u64; 7] = [1, 2, 3, 4, 6, 8, 12];
const SECONDS_PER_MINUTE: u64 = 60;
const SECONDS_PER_HOUR: u64 = 3600;
const SECONDS_PER_DAY: u64 = 86_400;
const DAYS_PER_WEEK: u64 = 7;
/// 1970-01-01 was a Thursday: day 5 when Sunday is 1.
const EPOCH_WEEKDAY: i64 = 5;

/// `--every` as an EventBridge cron, starting from `now` (seconds since the
/// epoch) so the first run comes one interval after the watch is made.
pub fn every_cron(every: &str, now: i64) -> Result<String> {
    let seconds = parse_timeout(every)
        .map_err(|why| anyhow::anyhow!("--every {every}: {why}"))?
        .as_secs();
    let minute = now.div_euclid(60).rem_euclid(60) as u64;
    let hour = now.div_euclid(3600).rem_euclid(24) as u64;
    let unsupported = || {
        anyhow::anyhow!(
            "--every {every} can't be a schedule; use 5m, 10m, 15m, 20m, 30m, 1h, 2h, 3h, 4h, 6h, 8h, 12h, 1d or 7d"
        )
    };
    if seconds % SECONDS_PER_DAY == 0 {
        let days = seconds / SECONDS_PER_DAY;
        return match days {
            1 => Ok(format!("cron({minute} {hour} * * ? *)")),
            DAYS_PER_WEEK => {
                let weekday =
                    (now.div_euclid(SECONDS_PER_DAY as i64) + EPOCH_WEEKDAY - 1).rem_euclid(7) + 1;
                Ok(format!("cron({minute} {hour} ? * {weekday} *)"))
            }
            _ => Err(unsupported()),
        };
    }
    if seconds % SECONDS_PER_HOUR == 0 {
        let hours = seconds / SECONDS_PER_HOUR;
        if !HOUR_STEPS.contains(&hours) {
            return Err(unsupported());
        }
        return Ok(format!("cron({minute} {}/{hours} * * ? *)", hour % hours));
    }
    if seconds % SECONDS_PER_MINUTE == 0 {
        let minutes = seconds / SECONDS_PER_MINUTE;
        if !MINUTE_STEPS.contains(&minutes) {
            return Err(unsupported());
        }
        return Ok(format!("cron({}/{minutes} * * * ? *)", minute % minutes));
    }
    Err(unsupported())
}

/// `gz-quality-watch-<table>-<id>`: a valid job and schedule name (letters,
/// digits and single hyphens, starting with a letter), the table cut short
/// to fit the schedule name limit.
pub fn watch_name(table: &LakehouseTable, id: &str) -> String {
    let slug_chars = MAX_NAME_CHARS - NAME_PREFIX.len() - id.len() - 1;
    let mut slug = String::new();
    for c in table.to_string().chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug: String = slug.trim_matches('-').chars().take(slug_chars).collect();
    let slug = slug.trim_end_matches('-');
    format!("{NAME_PREFIX}{slug}-{id}")
}

/// What a watch records about itself in its job's config.
#[derive(Debug, Clone, PartialEq)]
pub struct WatchRecord {
    pub table: String,
    pub every: String,
    pub cron: String,
    pub max_age: Option<String>,
    pub rules_file: Option<String>,
    pub fail_on: String,
    pub marker: String,
    pub notify_email: Vec<String>,
}

impl WatchRecord {
    pub fn to_json(&self) -> Value {
        json!({
            "version": METADATA_VERSION,
            "table": self.table,
            "every": self.every,
            "cron": self.cron,
            "maxAge": self.max_age,
            "rulesFile": self.rules_file,
            "failOn": self.fail_on,
            "marker": self.marker,
            "notifyEmail": self.notify_email,
        })
    }

    /// From a saved job's `config`; `None` when the job isn't a watch's.
    pub fn from_config(config: &Value) -> Option<Self> {
        let record = &config[METADATA_FIELD];
        let text = |field: &str| record[field].as_str().map(str::to_string);
        Some(Self {
            table: text("table")?,
            every: text("every").unwrap_or_default(),
            cron: text("cron").unwrap_or_default(),
            max_age: text("maxAge"),
            rules_file: text("rulesFile"),
            fail_on: text("failOn").unwrap_or_else(|| "error".into()),
            marker: text("marker")?,
            notify_email: record["notifyEmail"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|e| e.as_str().map(str::to_string))
                .collect(),
        })
    }

    /// The schedule's description, which the web app's Scheduler shows.
    pub fn description(&self) -> String {
        let age = self
            .max_age
            .as_ref()
            .map(|age| format!(", last commit within {age}"))
            .unwrap_or_default();
        format!(
            "gz quality watch of {} every {}{age}",
            self.table, self.every
        )
    }
}

/// `POST /schedule/{id}/email` for the failure email (schedule.schema.mjs
/// `NotificationConfigurationAddSchema`). The first address gets it, the
/// rest are copied; placeholders are the runtime's (`${ERR}`, ...).
pub fn failure_email(
    failure_type_id: i64,
    from: &str,
    to: &[String],
    table: &str,
    watch_id: &str,
) -> Result<Value> {
    let Some((first, rest)) = to.split_first() else {
        bail!("--notify-email needs an address");
    };
    for address in to.iter().chain(std::iter::once(&from.to_string())) {
        if !looks_like_email(address) {
            bail!("{address} is not an email address");
        }
    }
    Ok(json!({
        "notificationTypeId": failure_type_id,
        "fromEmail": from,
        "toEmail": first,
        "ccEmail": rest.join("; "),
        "subject": format!("Quality watch failed: {table}"),
        "messageBody": format!(
            "The scheduled quality check of {table} failed at ${{ENDTM}} \
             (job ${{JOBNM}}, project ${{PRJNM}}, site ${{SITENM}}).\n\n${{ERR}}\n\n\
             See it with: gz quality watch show {watch_id}"
        ),
        "attachLogs": false,
    }))
}

fn looks_like_email(text: &str) -> bool {
    let Some((user, domain)) = text.split_once('@') else {
        return false;
    };
    !user.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !text.chars().any(|c| c.is_whitespace() || c == ';')
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-11-18 (a Wednesday) 09:37:12 UTC.
    const NOW: i64 = 1_794_994_632;

    #[test]
    fn intervals_become_crons_that_start_now() {
        assert_eq!(every_cron("15m", NOW).unwrap(), "cron(7/15 * * * ? *)");
        assert_eq!(every_cron("1h", NOW).unwrap(), "cron(37 0/1 * * ? *)");
        assert_eq!(every_cron("6h", NOW).unwrap(), "cron(37 3/6 * * ? *)");
        assert_eq!(every_cron("1d", NOW).unwrap(), "cron(37 9 * * ? *)");
        assert_eq!(every_cron("24h", NOW).unwrap(), "cron(37 9 * * ? *)");
        assert_eq!(every_cron("7d", NOW).unwrap(), "cron(37 9 ? * 4 *)");
        for cron in ["5m", "1h", "1d", "7d"].map(|e| every_cron(e, NOW).unwrap()) {
            assert!(cron.len() >= 17, "{cron} is too short for the Schedule API");
        }
    }

    #[test]
    fn intervals_a_cron_cant_keep_are_refused() {
        for every in ["7m", "5h", "2d", "90s", "1", "soon"] {
            assert!(every_cron(every, NOW).is_err(), "{every}");
        }
    }

    #[test]
    fn names_are_valid_job_and_schedule_names() {
        let table = LakehouseTable::parse("GZWarehousePROD.DemoDB.ProductionMetrics").unwrap();
        assert_eq!(
            watch_name(&table, "1a2b3c4d"),
            "gz-quality-watch-gzwarehouseprod-demodb-productionmetri-1a2b3c4d"
        );
        assert_eq!(watch_name(&table, "1a2b3c4d").len(), MAX_NAME_CHARS);
        let odd = LakehouseTable::parse("w.`odd--name`.x_y").unwrap();
        let name = watch_name(&odd, "00ff00ff");
        assert!(!name.contains("--"), "{name}");
        assert!(name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    }

    #[test]
    fn the_record_round_trips_through_the_job_config() {
        let record = WatchRecord {
            table: "W.d.t".into(),
            every: "1h".into(),
            cron: "cron(37 0/1 * * ? *)".into(),
            max_age: Some("24h".into()),
            rules_file: None,
            fail_on: "error".into(),
            marker: "GZW-1a2b".into(),
            notify_email: vec!["a@b.io".into()],
        };
        let config = json!({"connectionNames": ["x"], METADATA_FIELD: record.to_json()});
        assert_eq!(WatchRecord::from_config(&config), Some(record.clone()));
        assert_eq!(WatchRecord::from_config(&json!({})), None);
        assert_eq!(
            record.description(),
            "gz quality watch of W.d.t every 1h, last commit within 24h"
        );
    }

    #[test]
    fn the_failure_email_copies_the_rest_and_uses_runtime_placeholders() {
        let to = vec!["a@b.io".to_string(), "c@d.io".to_string()];
        let body = failure_email(1003, "ops@b.io", &to, "W.d.t", "1041").unwrap();
        assert_eq!(body["toEmail"], "a@b.io");
        assert_eq!(body["ccEmail"], "c@d.io");
        assert_eq!(body["notificationTypeId"], 1003);
        let message = body["messageBody"].as_str().unwrap();
        assert!(message.contains("${ERR}") && message.contains("${JOBNM}"));
        assert!(message.ends_with("gz quality watch show 1041"));
        assert!(failure_email(1003, "ops@b.io", &["nope".into()], "t", "1").is_err());
        assert!(failure_email(1003, "ops@b.io", &[], "t", "1").is_err());
    }
}
