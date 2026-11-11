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

static WITH_RECORDS: ScreenSpec = ScreenSpec {
    noun: "run",
    identity: Identity {
        id: &["runId"],
        title: &["runId"],
    },
    detail: Some(DetailSpec {
        related: &[
            crate::tui::screen::RelatedSpec {
                title: "Governance",
                source: Source::get("agents", "/runs/{runId}/governance"),
                columns: &[crate::tui::screen::Column::new(
                    "Safety",
                    &["safetyStatus"],
                    Format::Text,
                )],
                when: None,
                empty: "No governance record",
                limit: 1,
            },
            crate::tui::screen::RelatedSpec {
                title: "Trace",
                source: Source::get("agents", "/runs/{runId}/observability"),
                columns: &[crate::tui::screen::Column::new(
                    "Steps",
                    &["cycleCount"],
                    Format::Number,
                )],
                when: None,
                empty: "No detailed observability",
                limit: 1,
            },
        ],
        ..DetailSpec::BLANK
    }),
    ..ScreenSpec::BLANK
};

#[test]
fn related_objects_show_as_one_row_and_missing_records_as_empty() {
    let mut app = app();
    app.push_view(Box::new(DetailView::new(&WITH_RECORDS, run_row()).unwrap()));
    let fetches: Vec<_> = app
        .outbox
        .iter()
        .filter_map(|r| match r {
            Request::Fetch(f) => Some(f.clone()),
            _ => None,
        })
        .collect();
    let governance = fetches
        .iter()
        .find(|f| f.path.ends_with("/governance"))
        .unwrap();
    let trace = fetches
        .iter()
        .find(|f| f.path.ends_with("/observability"))
        .unwrap();
    app.fetched(
        governance.id,
        governance.target.clone(),
        Ok(json!({"safetyStatus": "passed"})),
    );
    app.fetched(
        trace.id,
        trace.target.clone(),
        Err("GET u → HTTP 404 Not Found: Run observability is not available".into()),
    );
    let screen = draw(&mut app, 100, 24);
    assert!(screen.contains("passed"), "{screen}");
    assert!(screen.contains("No detailed observability"), "{screen}");
    assert!(!screen.contains("deleted"), "{screen}");
}

static WITH_TEXT: ScreenSpec = ScreenSpec {
    noun: "run",
    identity: Identity {
        id: &["runId"],
        title: &["runId"],
    },
    detail: Some(DetailSpec {
        sections: &[
            Section {
                title: "Prompt",
                body: crate::tui::screen::SectionBody::Text(&["inputText"]),
            },
            Section {
                title: "Answer",
                body: crate::tui::screen::SectionBody::Text(&["outputText"]),
            },
        ],
        ..DetailSpec::BLANK
    }),
    ..ScreenSpec::BLANK
};

#[test]
fn text_sections_wrap_long_text_and_skip_empty_text() {
    let mut row = run_row();
    row.row = json!({"runId": "r-42", "inputText": "alpha beta gamma delta\nsecond line", "outputText": ""});
    let mut app = app();
    app.push_view(Box::new(DetailView::new(&WITH_TEXT, row).unwrap()));
    let screen = draw(&mut app, 20, 14);
    assert!(screen.contains("alpha beta"), "{screen}");
    assert!(screen.contains("gamma delta"), "{screen}");
    assert!(screen.contains("second line"), "{screen}");
    assert!(!screen.contains("Answer"), "{screen}");
}

const LONG_NAME: &str =
    "hdfc-14-probe-model-artifact-roundtrip-with-a-suffix-long-enough-to-wrap-at-eighty-cols";

/// The job run screen with `name`'s details open on top of it.
fn open_run(name: &str) -> crate::tui::app::App {
    let mut row = run_row();
    row.row = json!({"runId": name, "status": "RUNNING"});
    let mut app = app();
    app.open_and_focus(resource("DE/ML", "Job runs"));
    app.push_view(Box::new(DetailView::new(&RUNS, row).unwrap()));
    answer(&mut app, json!({"status": "FAILED", "durationMs": 1000}));
    app
}

#[test]
fn the_name_appears_once_inside_the_frame() {
    let name = "hdfc-14-probe-model-artifact-roundtrip";
    let mut app = open_run(name);
    let screen = draw(&mut app, 150, 42);
    let mut lines = screen.lines();
    let top_bar = lines.next().unwrap();
    assert!(top_bar.contains(&format!("Job runs › {name}")), "{top_bar}");
    let frame_title = lines.next().unwrap();
    assert!(frame_title.contains("╭ Details "), "{frame_title}");
    let inside: Vec<&str> = lines.filter(|l| l.contains(name)).collect();
    assert_eq!(inside.len(), 1, "{screen}");
    assert!(inside[0].contains(&format!("{name}  ● Failed")), "{screen}");
}

#[test]
fn raw_json_says_so_in_the_frame_title() {
    let mut app = open_run("r-42");
    press(&mut app, &[KeyCode::Char('J')]);
    let screen = draw(&mut app, 100, 20);
    let frame_title = screen.lines().nth(1).unwrap();
    assert!(frame_title.contains("╭ Raw JSON "), "{frame_title}");
    assert!(frame_title.contains("J formatted"), "{frame_title}");
}

#[test]
fn a_long_name_wraps_whole_and_keeps_its_status() {
    let mut app = open_run(LONG_NAME);
    let screen = draw(&mut app, 80, 20);
    let header: String = screen
        .lines()
        .skip(2)
        .take(2)
        .map(|l| l.trim_matches(|c| c == '│' || c == ' '))
        .collect();
    assert!(
        header.starts_with(LONG_NAME),
        "the whole name, across lines\n{screen}"
    );
    assert!(header.ends_with("● Failed"), "{screen}");
}

#[test]
fn a_long_name_wraps_after_a_hyphen_not_mid_word() {
    const NAME: &str =
        "hdfc-14-probe-model-artifact-roundtrip-keeps-going-on-past-the-right-edge-of-an-eighty-col-screen";
    let mut app = open_run(NAME);
    let screen = draw(&mut app, 80, 20);
    let first = screen
        .lines()
        .nth(2)
        .unwrap()
        .trim_matches(|c| c == '│' || c == ' ');
    assert!(
        NAME.starts_with(first) && first.ends_with('-'),
        "the first line ends at a hyphen\n{screen}"
    );
}

#[test]
fn a_long_name_in_the_top_bar_ends_with_an_ellipsis_before_the_sign_in() {
    let mut app = open_run(LONG_NAME);
    let screen = draw(&mut app, 80, 20);
    let top_bar = screen.lines().next().unwrap();
    assert!(top_bar.contains("…  ● not signed in"), "{top_bar}");
}
