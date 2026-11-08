//! Running the background news check and turning its findings into
//! status-bar notices.

use serde_json::Value;

use crate::tui::fetch::FetchTarget;
use crate::tui::watch::Feed;

use super::App;

impl App {
    /// Asks about each feed when the watcher says it is time.
    pub(super) fn poll_watch(&mut self) {
        let now = self.now;
        for feed in self.watcher.due(now.instant) {
            let id = self.fetch_get(
                FetchTarget::Watch(feed),
                feed.service(),
                feed.path(now.epoch_ms),
            );
            self.watcher.sent(feed, id);
        }
    }

    pub(super) fn watch_fetched(&mut self, feed: Feed, id: u64, result: Result<Value, String>) {
        if let Some(notice) = self.watcher.answered(feed, id, result) {
            self.notifier.push(notice);
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use serde_json::json;

    use super::*;
    use crate::tui::app::test_support::*;
    use crate::tui::app::Tone;
    use crate::tui::fetch::{FetchReq, Request};
    use crate::tui::watch::{EVERY, FIRST_LOOK_AFTER};

    fn watch_fetch(app: &App, feed: Feed) -> FetchReq {
        app.outbox
            .iter()
            .rev()
            .find_map(|r| match r {
                Request::Fetch(f) if f.target == FetchTarget::Watch(feed) => Some(f.clone()),
                _ => None,
            })
            .expect("the watcher asked")
    }

    fn reply(app: &mut App, feed: Feed, result: Result<Value, String>) {
        let req = watch_fetch(app, feed);
        app.fetched(req.id, req.target, result);
    }

    #[test]
    fn new_failures_raise_a_notice_that_jumps_to_the_run() {
        let mut app = app();
        app.on_tick(app.now);
        let start = app.now.advanced(FIRST_LOOK_AFTER);
        app.on_tick(start);
        reply(&mut app, Feed::FailedRuns, Ok(json!({"items": []})));
        reply(&mut app, Feed::Approvals, Ok(json!({"items": []})));
        assert!(app.notifier.showing().is_none());

        app.on_tick(start.advanced(EVERY));
        let failure = json!({"items": [{"status": "failed", "executionId": "2cb3",
            "jobName": "ChallanReceiptBulk", "projectName": "Invoice-Extractor"}]});
        reply(&mut app, Feed::FailedRuns, Ok(failure));
        app.on_tick(start.advanced(EVERY));
        let notice = app.notifier.showing().expect("a notice");
        assert_eq!(
            notice.text,
            "Job run failed: ChallanReceiptBulk (Invoice-Extractor)"
        );
        assert!(draw(&mut app, 120, 20).contains("^g open"));

        app.on_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL));
        assert_eq!(app.active, resource("Catalog", "Job usage"));
        answer(&mut app, json!({"items": [{"executionId": "2cb3"}]}));
        let row = app.view().unwrap().grid().selected_raw().unwrap().clone();
        assert_eq!(row["executionId"], "2cb3");
    }

    #[test]
    fn watcher_failures_never_touch_the_status_bar() {
        let mut app = app();
        let before = app.status.text.clone();
        app.on_tick(app.now);
        app.on_tick(app.now.advanced(FIRST_LOOK_AFTER));
        reply(
            &mut app,
            Feed::FailedRuns,
            Err("request failed: GET x: error sending request: dns error".into()),
        );
        assert_eq!(app.status.text, before);
        assert_ne!(app.status.tone, Tone::Error);
    }
}
