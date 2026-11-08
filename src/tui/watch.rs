//! A slow background check for news worth interrupting for: job runs that
//! failed and agent versions newly waiting for your review. The first check
//! only learns what is already there; later checks raise one status-bar
//! notice per kind of news, with a count when several arrive at once, and
//! `ctrl+g` jumps to the source.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::tui::errors;
use crate::tui::fields;
use crate::tui::format;
use crate::tui::notify::{Jump, Notification};
use crate::tui::screen::{Goto, Select};
use crate::tui::theme::BadgeTone;
use crate::tui::views::home::{tiles, HomeView, APPROVAL_GOTO, RUN_GOTO};
use crate::tui::views::Poller;

/// How often to look. Low on purpose: the ETL list is uncached and slow.
pub const EVERY: Duration = Duration::from_secs(3 * 60);
/// The first look waits this long after launch.
pub const FIRST_LOOK_AFTER: Duration = Duration::from_secs(10);
/// Failed runs are looked for in this window; it only has to cover the
/// time between two checks, with room for a laptop waking from sleep.
const FAILED_WINDOW_MS: i64 = 24 * 60 * 60 * 1000;
/// Names spelled out in a notice before "and N more".
const NAMES_SHOWN: usize = 2;

/// What the watcher follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Feed {
    FailedRuns,
    Approvals,
}

const FEEDS: [Feed; 2] = [Feed::FailedRuns, Feed::Approvals];

impl Feed {
    pub fn service(self) -> &'static str {
        match self {
            Feed::FailedRuns => "etl",
            Feed::Approvals => "agents",
        }
    }

    pub fn path(self, now_ms: i64) -> String {
        match self {
            Feed::FailedRuns => format!(
                "/etl/consumption-metrics?status=failed&workloadType=job&from={}&limit=20",
                format::iso8601_utc(now_ms - FAILED_WINDOW_MS)
            ),
            Feed::Approvals => format!(
                "/v1/approval-requests?status=waiting_for_approval&limit={}",
                tiles::APPROVALS_PAGE
            ),
        }
    }

    /// Row fields that identify one item of this feed.
    fn id_field(self) -> &'static str {
        match self {
            Feed::FailedRuns => "executionId",
            Feed::Approvals => "id",
        }
    }
}

#[derive(Debug)]
pub struct Watcher {
    poller: Poller,
    /// When the watcher first ticked; the first look waits a moment so it
    /// doesn't compete with the screens loading at launch.
    started: Option<Instant>,
    /// The first look (which only learns what exists) has been taken.
    looked: bool,
    /// Ids in each feed's last answer; absent until the first answer.
    seen: HashMap<Feed, HashSet<String>>,
    /// Feeds the user may not see; never asked again this session.
    muted: HashSet<Feed>,
    awaiting: HashMap<Feed, u64>,
}

impl Default for Watcher {
    fn default() -> Self {
        Self {
            poller: Poller::every(EVERY),
            started: None,
            looked: false,
            seen: HashMap::new(),
            muted: HashSet::new(),
            awaiting: HashMap::new(),
        }
    }
}

impl Watcher {
    /// Feeds to ask about at `now`: all of them shortly after launch (to
    /// learn what is already there), then once per [`EVERY`].
    pub fn due(&mut self, now: Instant) -> Vec<Feed> {
        let due = match (self.started, self.looked) {
            (None, _) => {
                self.started = Some(now);
                false
            }
            (Some(_), true) => self.poller.due(now),
            (Some(started), false) if now.duration_since(started) >= FIRST_LOOK_AFTER => {
                self.looked = true;
                self.poller.restart(now);
                true
            }
            (Some(_), false) => false,
        };
        if !due {
            return Vec::new();
        }
        FEEDS
            .into_iter()
            .filter(|feed| !self.muted.contains(feed) && !self.awaiting.contains_key(feed))
            .collect()
    }

    pub fn sent(&mut self, feed: Feed, id: u64) {
        self.awaiting.insert(feed, id);
    }

    /// Takes an answer and returns the notice to raise, if it has news.
    pub fn answered(
        &mut self,
        feed: Feed,
        id: u64,
        result: Result<Value, String>,
    ) -> Option<Notification> {
        if self.awaiting.get(&feed) != Some(&id) {
            return None;
        }
        self.awaiting.remove(&feed);
        let answer = match result {
            Ok(answer) => answer,
            Err(err) => {
                // Not allowed stays not allowed; anything else may pass, so
                // the next check tries again. Either way, stay quiet.
                if errors::is_access_denied(&err, feed.service()) {
                    self.muted.insert(feed);
                }
                return None;
            }
        };
        let rows = news_rows(feed, &answer);
        let ids: HashSet<String> = rows.iter().filter_map(|row| row_id(feed, row)).collect();
        let previous = self.seen.insert(feed, ids)?;
        let fresh: Vec<&Value> = rows
            .iter()
            .filter(|row| row_id(feed, row).is_some_and(|id| !previous.contains(&id)))
            .collect();
        notice(feed, &fresh)
    }
}

/// The rows of an answer that count as news (failed-run answers are
/// already filtered by status, but a dataset or older shape may slip in).
fn news_rows(feed: Feed, answer: &Value) -> Vec<Value> {
    let rows = answer["items"].as_array().cloned().unwrap_or_default();
    match feed {
        Feed::FailedRuns => rows
            .into_iter()
            .filter(|row| {
                row["status"]
                    .as_str()
                    .is_some_and(|s| s.eq_ignore_ascii_case("failed"))
            })
            .collect(),
        Feed::Approvals => rows,
    }
}

fn row_id(feed: Feed, row: &Value) -> Option<String> {
    fields::lookup(row, feed.id_field()).and_then(fields::id_text)
}

fn notice(feed: Feed, fresh: &[&Value]) -> Option<Notification> {
    let first = *fresh.first()?;
    let notification = match (feed, fresh.len()) {
        (Feed::FailedRuns, 1) => {
            let run = tiles::job_run(first);
            let project = if run.project.is_empty() {
                String::new()
            } else {
                format!(" ({})", run.project)
            };
            Notification::new(
                BadgeTone::Bad,
                format!("Job run failed: {}{project}", run.job),
            )
            .jump_to(Jump::Follow(RUN_GOTO, first.clone()))
        }
        (Feed::FailedRuns, count) => {
            let names = names(fresh, |row| tiles::job_run(row).job);
            Notification::new(BadgeTone::Bad, format!("{count} job runs failed: {names}"))
                .jump_to(Jump::View(Rc::new(|| Box::new(HomeView::new()))))
        }
        (Feed::Approvals, 1) => {
            let approval = tiles::approval(first);
            let version = approval
                .version
                .map(|v| format!(" v{v}"))
                .unwrap_or_default();
            Notification::new(
                BadgeTone::Warn,
                format!("{}{version} is waiting for your review", approval.agent),
            )
            .jump_to(Jump::Follow(APPROVAL_GOTO, first.clone()))
        }
        (Feed::Approvals, count) => {
            let names = names(fresh, |row| tiles::approval(row).agent);
            Notification::new(
                BadgeTone::Warn,
                format!("{count} agent versions are waiting for your review: {names}"),
            )
            .jump_to(Jump::Follow(
                Goto {
                    target: APPROVAL_GOTO.target,
                    select: Select::Open,
                },
                Value::Null,
            ))
        }
    };
    Some(notification)
}

/// "a, b and 3 more".
fn names(rows: &[&Value], name: impl Fn(&Value) -> String) -> String {
    let shown: Vec<String> = rows.iter().take(NAMES_SHOWN).map(|r| name(r)).collect();
    let rest = rows.len().saturating_sub(NAMES_SHOWN);
    match rest {
        0 => shown.join(" and "),
        _ => format!("{} and {rest} more", shown.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn failed(id: &str, job: &str) -> Value {
        json!({"status": "failed", "executionId": id, "jobName": job, "projectName": "ETL"})
    }

    /// A watcher that has seen the app start, and the moment of its first look.
    fn launched() -> (Watcher, Instant) {
        let launch = Instant::now();
        let mut watcher = Watcher::default();
        assert!(watcher.due(launch).is_empty(), "nothing at launch itself");
        assert!(watcher.due(launch + FIRST_LOOK_AFTER / 2).is_empty());
        (watcher, launch + FIRST_LOOK_AFTER)
    }

    /// Runs one check of `feed` that answers `answer`.
    fn check(
        watcher: &mut Watcher,
        now: Instant,
        feed: Feed,
        answer: Value,
    ) -> Option<Notification> {
        let due = watcher.due(now);
        assert!(due.contains(&feed), "{feed:?} wasn't due");
        watcher.sent(feed, 7);
        watcher.answered(feed, 7, Ok(answer))
    }

    #[test]
    fn the_first_check_learns_what_is_already_there() {
        let (mut watcher, start) = launched();
        assert_eq!(
            watcher.due(start),
            FEEDS.to_vec(),
            "looks soon after launch"
        );
        watcher.sent(Feed::FailedRuns, 1);
        let baseline = watcher.answered(
            Feed::FailedRuns,
            1,
            Ok(json!({"items": [failed("a", "old")]})),
        );
        assert!(baseline.is_none(), "old failures are not news");
        assert!(watcher.due(start + EVERY / 2).is_empty(), "then waits");
    }

    #[test]
    fn a_new_failure_is_news_once() {
        let (mut watcher, start) = launched();
        check(
            &mut watcher,
            start,
            Feed::FailedRuns,
            json!({"items": [failed("a", "old")]}),
        );
        let next = start + EVERY;
        let answer = json!({"items": [failed("b", "ChallanReceiptBulk"), failed("a", "old")]});
        let notice = check(&mut watcher, next, Feed::FailedRuns, answer.clone()).unwrap();
        assert_eq!(notice.text, "Job run failed: ChallanReceiptBulk (ETL)");
        assert_eq!(notice.tone, BadgeTone::Bad);
        match notice.jump {
            Some(Jump::Follow(goto, row)) => {
                assert_eq!(goto.target, RUN_GOTO.target);
                assert_eq!(row["executionId"], "b");
            }
            other => panic!("jumps to the run, got {other:?}"),
        }
        assert!(
            check(&mut watcher, next + EVERY, Feed::FailedRuns, answer).is_none(),
            "the same failure is not repeated"
        );
    }

    #[test]
    fn several_arrivals_make_one_notice_with_a_count() {
        let (mut watcher, start) = launched();
        check(&mut watcher, start, Feed::Approvals, json!({"items": []}));
        let approvals: Vec<Value> = ["judge", "coach", "watch"]
            .iter()
            .enumerate()
            .map(|(i, name)| json!({"id": i, "agentVersion": 2, "definitionSnapshot": {"name": name}}))
            .collect();
        let notice = check(
            &mut watcher,
            start + EVERY,
            Feed::Approvals,
            json!({ "items": approvals }),
        )
        .unwrap();
        assert_eq!(
            notice.text,
            "3 agent versions are waiting for your review: judge, coach and 1 more"
        );
        assert!(
            matches!(notice.jump, Some(Jump::Follow(goto, _)) if matches!(goto.select, Select::Open))
        );
    }

    #[test]
    fn a_single_approval_names_agent_and_version() {
        let (mut watcher, start) = launched();
        check(&mut watcher, start, Feed::Approvals, json!({"items": []}));
        let answer = json!({"items": [{"id": 271, "agentVersion": 3, "definitionSnapshot": {"name": "rlo-pair-judge"}}]});
        let notice = check(&mut watcher, start + EVERY, Feed::Approvals, answer).unwrap();
        assert_eq!(notice.text, "rlo-pair-judge v3 is waiting for your review");
    }

    #[test]
    fn feeds_the_user_may_not_see_are_dropped_quietly() {
        let (mut watcher, start) = launched();
        watcher.due(start);
        watcher.sent(Feed::Approvals, 3);
        let denied =
            r#"GET u → HTTP 401 Unauthorized: You do not have "Approve" permission on agent."#;
        assert!(watcher
            .answered(Feed::Approvals, 3, Err(denied.into()))
            .is_none());
        watcher.sent(Feed::FailedRuns, 4);
        let outage = "GET u → HTTP 503 Service Unavailable";
        assert!(watcher
            .answered(Feed::FailedRuns, 4, Err(outage.into()))
            .is_none());
        assert_eq!(
            watcher.due(start + EVERY),
            vec![Feed::FailedRuns],
            "an outage is retried; a permission denial is not"
        );
    }

    #[test]
    fn stale_answers_and_busy_feeds_are_skipped() {
        let (mut watcher, start) = launched();
        watcher.due(start);
        watcher.sent(Feed::FailedRuns, 9);
        assert!(watcher
            .answered(Feed::FailedRuns, 8, Ok(json!({"items": []})))
            .is_none());
        assert!(
            !watcher.due(start + EVERY).contains(&Feed::FailedRuns),
            "no second request while one is out"
        );
    }

    #[test]
    fn the_failed_runs_query_asks_only_for_failed_jobs() {
        let path = Feed::FailedRuns.path(crate::tui::app::test_support::TEST_NOW_MS);
        assert_eq!(
            path,
            "/etl/consumption-metrics?status=failed&workloadType=job&from=2026-11-07T12:00:00Z&limit=20"
        );
    }
}
