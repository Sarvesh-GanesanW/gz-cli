use std::time::Duration;

use serde_json::json;

use super::*;
use crate::tui::app::test_support::*;
use crate::tui::fetch::{FetchTarget, Request};
use crate::tui::format::{Format, StatusMap};
use crate::tui::nav::Source;
use crate::tui::screen::{Identity, Section};
use crate::tui::theme::BadgeTone;

static RUN_STATUS: StatusMap = StatusMap {
    entries: &[
        ("RUNNING", "Running", BadgeTone::Info),
        ("FAILED", "Failed", BadgeTone::Bad),
    ],
    fallback: BadgeTone::Muted,
};

static RUNS: ScreenSpec = ScreenSpec {
    noun: "run",
    identity: Identity {
        id: &["runId"],
        title: &["runId"],
    },
    detail: Some(DetailSpec {
        load: Some(Source::get("etl", "/runs/{runId}")),
        badge: Some((&["status"], &RUN_STATUS)),
        sections: &[Section {
            title: "Timing",
            body: crate::tui::screen::SectionBody::Fields(&[crate::tui::screen::DetailField::new(
                "Took",
                &["durationMs"],
                Format::Duration(format::TimeUnit::Millis),
            )]),
        }],
        poll: Some(Duration::from_secs(10)),
        ..DetailSpec::BLANK
    }),
    ..ScreenSpec::BLANK
};

fn run_row() -> RowContext {
    RowContext {
        row: json!({"runId": "r-42", "status": "RUNNING"}),
        title: Some("r-42".into()),
        noun: "run",
        spec: Some(&RUNS),
        parents: Vec::new(),
        ids: vec!["r-42".into()],
        resource: None,
        view: None,
    }
}

fn run_fetches(app: &crate::tui::app::App) -> usize {
    app.outbox
        .iter()
        .filter(|r| matches!(r, Request::Fetch(f) if f.path == "/runs/r-42"))
        .count()
}

#[test]
fn header_shows_the_status_badge_and_loaded_fields() {
    let mut app = app();
    app.push_view(Box::new(DetailView::new(&RUNS, run_row()).unwrap()));
    assert!(draw(&mut app, 80, 20).contains("Loading details…"));
    answer(&mut app, json!({"status": "FAILED", "durationMs": 245000}));
    let screen = draw(&mut app, 80, 20);
    assert!(screen.contains("r-42  ● Failed"), "{screen}");
    assert!(screen.contains("Took  4m 05s"), "{screen}");
}

#[test]
fn load_failures_are_explained_in_place() {
    let mut app = app();
    app.push_view(Box::new(DetailView::new(&RUNS, run_row()).unwrap()));
    let req = last_fetch(&app);
    app.fetched(
        req.id,
        req.target,
        Err("GET u → HTTP 404 Not Found: gone".into()),
    );
    let screen = draw(&mut app, 100, 20);
    assert!(
        screen.contains("This run was deleted or never existed. (gone)"),
        "{screen}"
    );
}

#[test]
fn live_details_reload_on_their_interval_until_closed() {
    let mut app = app();
    let start = app.now;
    app.push_view(Box::new(DetailView::new(&RUNS, run_row()).unwrap()));
    answer(&mut app, json!({"status": "RUNNING"}));
    app.on_tick(start);
    app.on_tick(start.advanced(Duration::from_secs(9)));
    assert_eq!(run_fetches(&app), 1);
    app.on_tick(start.advanced(Duration::from_secs(10)));
    assert_eq!(run_fetches(&app), 2);
    assert!(matches!(last_fetch(&app).target, FetchTarget::View(_)));
    app.views.pop();
    app.on_tick(start.advanced(Duration::from_secs(60)));
    assert_eq!(run_fetches(&app), 2);
}

#[test]
fn screens_without_a_detail_spec_have_no_detail_view() {
    assert!(DetailView::new(&ScreenSpec::BLANK, run_row()).is_none());
}
