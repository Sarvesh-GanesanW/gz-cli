# gz TUI

`gz tui` is a ratatui app. Screens are described as data (screen specs);
full-screen views are one type each on a stack. Colours come only from
`theme.rs`. App logic runs without a TTY, so everything is tested with
plain key presses and ratatui's `TestBackend`.

## Map

| Path | What it holds |
|---|---|
| `specs/<module>.rs` | One sidebar module each: its resources and their `ScreenSpec`s. **Screen work happens here.** |
| `nav.rs` | `Module`, `Resource`, `GridSpec`, `Drill`, `Source`, `Verb`; `MODULES` lists the spec files in sidebar order. |
| `screen/` | The spec vocabulary: `ScreenSpec`, `Column`, `Identity`, `DetailSpec`, `Section`, `RelatedSpec`, `ActionSpec`, `FormSpec`, `LinkSpec`, `Cond`, `KeyHelp`. |
| `format.rs` | Value formatters (`Format`), status badges (`StatusMap`), secret masking, key humanizing. |
| `template.rs` | Request templates: `{0}`, `{Id}`, `{config.siteName}`, `{a\|b}`, `{$site}`. |
| `fields.rs` | Field lookup by dotted path and alternatives (`FieldPath`). |
| `errors.rs` | Every error message: HTTP status → plain words, keeping the server's message. |
| `views/` | The `View` trait, `Ctx`, `Effect`, `ViewStack`, `Poller`; `views/detail/` is the detail view, `views/home/` the dashboard. |
| `overlay/` | Pure state machines for the action menu, confirm dialog and form. |
| `notify.rs` | Status-bar notifications (`ctrl+g` jumps to their source). |
| `watch.rs` | The background news check: new failed job runs and approvals become notices. |
| `input.rs` | The key reader thread; paused while `Effect::HandOff` work (gz code from `F`) has the terminal. |
| `ui_state.rs` | What the TUI remembers between launches (`tui_state.json`, owner-only). |
| `help.rs` | The `?` overlay's data and search: grouped sections, `table`, `actions`, `for_screen`, reserved keys. |
| `app/` | `App` state, split by concern: `keys`, `mouse`, `navigation`, `history` (Back, Recent, places), `saved`, `requests`, `actions`, `links`, `views`, `chat`. |
| `ui/` | Drawing: `mod.rs` (frame, sidebar, grid, footer), `chat.rs`, `overlays.rs`, `help.rs`, `states.rs` (loading/empty/failed), `widgets.rs` (shared helpers). |
| `model.rs` | `Grid`: rows, filter, selection, sorting. |

`specs/connections.rs` is the reference conversion. Copy its shape.

## Working in parallel

Each screen lives in its own `specs/<module>.rs`, and each full-screen view
gets its own file under `views/`. To add a screen, an action, a form, a link,
a view or a help entry you only touch those files. The shared files
(`nav.rs`, `app/*`, `ui/mod.rs`, `screen/*`) need a change only when the
vocabulary itself grows (a new `Format`, a new `Effect`). Keep those changes
small and additive, and add new enum variants at the end.

## Add a screen spec

```rust
// specs/<module>.rs
pub static RUNS: ScreenSpec = ScreenSpec {
    noun: "run",                                   // "this run", "these runs"
    columns: &[
        Column::new("Run", &["runId"], Format::Text),
        Column::new("Status", &["status"], Format::Status(&RUN_STATUS)),
        Column::new("Started", &["startedAt", "createdAt"], Format::RelTime),
        Column::new("Cost", &["usage.cost.usd"], Format::Usd).width(9),
    ],
    sort: Some(Sort { field: &["startedAt"], descending: true }),
    identity: Identity { id: &["runId"], title: &["name", "runId"] },
    empty: "No runs yet",
    ..ScreenSpec::BLANK
};

pub const MODULE: Module = Module {
    title: "…",
    icon: "…",
    resources: &[screen("Runs", Source::get("agents", "/v1/runs"), &RUNS)],
};
```

- A `FieldPath` (`&["a", "b.c"]`) lists alternatives tried in order. Each one
  can be a dotted path, with array indexes as `bindings.0.name`.
- The id field drives drills, details, actions and links. It replaces the
  global `nav::ID_KEYS`, so a screen keyed on `assetId` or `scheduleName`
  just names it.
- Drill levels carry their own spec: `Drill { screen: Some(&TABLES), ..drill(...) }`.
  `GridSpec::screen_at(depth)` picks the right one. Drill templates can use
  parent fields by name (`/etl/{projectId}/jobs/{id}`).
- A screen without a spec keeps the generic table (all fields, JSON details).
- `poll: Some(Duration::from_secs(5))` reloads the list while it is on screen.
- Formats: `Text`, `RelTime`, `DateTime`, `Date`, `Bytes` (accepts `"4MB"`),
  `Duration(TimeUnit::Millis|Seconds)`, `Usd`, `Number`, `Count` (array
  length), `Bool`, `Status(&StatusMap)`, `Label(&[(code, label)])`,
  `Percent` (0–100 → "98.6%"), `Compact` ("4.4M"), `Cents` (integer cents),
  `Version` ("v10"), `Model` (`in.openai.gpt-5.6-luna` → "GPT-5.6 Luna").
  Missing values show `—`. Values that don't parse show as text.
- `rows: Some(&["modelMetrics"])` reads the rows from a named array when
  the answer is one object rather than a list or a known envelope.
- `shape: Some(fn(rows, now_ms) -> rows)` rewrites the whole list after it
  loads, e.g. grouping with `shape::group_by(rows, &["tableName"], "columns")`
  (one row per column → one row per table, members under `columns`). For
  fields computed per row use `enrich`.
- `paging: Some(Paging { size: 100, max: 500 })` loads a GET list a page at a
  time with `limit`/`offset`. The next page loads when the selection reaches
  the last row; `hasMore`/`nextOffset` or `total` in the answer say whether
  there is more, and the title shows "100 of 1,441 rows · ↓ more". A reload
  asks for every row already shown (up to `max`).
- `compose: Some(&COMPOSE)` builds the rows from several requests: the
  level's own source plus `with` sources, optionally `expand`ed once per row
  of one answer (each project's jobs), then `combine(&Parts) -> Combined`.
  A failed source still lets the others show; `Combined.failures` go to the
  status line. `Parts.parent` is the row drilled into. See
  `specs/catalog/lineage.rs` (a join) and `specs/catalog/data_catalog.rs`
  (a union with a fan-out).
- `open: Some(fn(&Value, &mut Ctx))` replaces Enter on a row (past the last
  drill) — a table opens its preview — while `i` still opens the details.
- A drill can apply only to some rows (`when: Some(Cond)`: folders drill,
  files open their details) and can `repeat` for every deeper level (folders
  in folders): `Drill { when: Some(FOLDER), repeats: true, ..drill(…) }`.

```rust
static RUN_STATUS: StatusMap = StatusMap {
    entries: &[("SUCCEEDED", "Succeeded", BadgeTone::Good), ("FAILED", "Failed", BadgeTone::Bad)],
    fallback: BadgeTone::Info,          // unknown values keep their text
};
```

## Add a detail view

Set `detail: Some(DetailSpec { … })` on the spec. Enter opens it on the last
drill level, and `i` opens it on any level.

```rust
detail: Some(DetailSpec {
    load: Some(Source::get("etl", "/runs/{runId}")),  // merged over the list row
    missing: "Run not found. It may have been deleted.", // when load answers []
    badge: Some((&["status"], &RUN_STATUS)),
    subtitle: &[(&["agentName"], Format::Text)],
    sections: &[
        Section { title: "Timing", body: SectionBody::Fields(&[
            DetailField::new("Took", &["durationMs"], Format::Duration(TimeUnit::Millis)),
            DetailField::new("Notes", &["notes"], Format::Text).optional(),
        ]) },
        Section { title: "Config", body: SectionBody::Object(&["config"]) }, // secrets masked
    ],
    related: &[RelatedSpec {
        title: "Steps",
        source: Source::get("agents", "/v1/runs/{runId}/steps"),
        columns: STEP_COLUMNS, when: None, empty: "No steps", limit: 20,
    }],
    copy: Some(CopySpec { label: "run URL", template: "https://…/runs/{runId}" }),
    poll: Some(Duration::from_secs(10)),             // live detail
    ..DetailSpec::BLANK
}),
```

`SectionBody::Text(&["instructions"])` shows long text wrapped, keeping its
line breaks.

Related blocks load after the details, so they can use detail fields. A
source that answers one object shows as one row. A 403 on a related block
hides that block, a 404 shows its `empty` text, and any other failure is
shown in place.
Keys: `J` toggles raw JSON, `y` copies the id, `Y` copies `copy`, `a` opens
the actions, `r` reloads, Esc goes back.

## Add an action

```rust
const DELETE: ActionSpec = ActionSpec::new('d', "Delete", ActionKind::Request(RequestSpec {
    source: Source::new("connections", Verb::Delete, "/connection/{Id}", None),
    success: "Connection deleted",          // used when the server sends no message
    after: After::RefreshAndClose,          // Refresh | RefreshAndClose | Stay
}))
.destructive("Delete connection {Name}? Datasets and jobs using it will break.")
.when(Cond { field: &["type"], any_of: &["AWSS3"] });
```

- List it in `actions: &[DELETE]`. Its key works on the table and in the
  detail view, and it shows in the `a` menu and in `?`.
- `.confirm("…")` asks first: Enter or `y`. `.destructive("…")` makes the
  user type the row's title (or press `D` when the row has none). The dialog
  also shows the exact request (`DELETE connections /connection/1000`).
- Templates are filled from form values, then the row (merged with its
  details in a detail view), then its parents. `{$site}` is the signed-in
  site. A missing field stops the action with "this row has no `x`".
- Failures go through `errors.rs`, e.g. "Delete failed: You don't have
  access to this connection. (server message)".
- `ActionKind::Navigate(Goto)` opens another screen.
- `ActionKind::CustomRow(fn(&RowContext, &mut Ctx))` is the same with the
  row's parents and drill ids (a job's project).
- `ActionKind::Custom(fn(&Value, &mut Ctx))` runs code: download, push a
  live view, and so on.
- `ActionKind::FormCustom(&FORM, fn(&Value row, &Value values, &mut Ctx))`
  asks first, then runs code with the values (a download's destination, a
  file to upload).
- Async work beyond one JSON request (a download to disk, an upload, a
  lakehouse session) runs with `ctx.run_task(label, |client| async { … })`.
  From a view its answer comes to `on_fetched`; from a row action it is shown
  (its `{"message"}`, or the failure text) and the row's screen and view are
  refreshed.
- Keys must not reuse `help::GRID_KEYS` or `help::DETAIL_KEYS`, and must be
  unique within the screen. A test checks every spec.

## Add a form

```rust
static RENAME: FormSpec = FormSpec { title: "Rename", submit: "Rename", fields: &[
    FormField::new("name", "New name", FieldKind::Text).required()
        .default("{Name}")
        .pattern("[A-Za-z][A-Za-z0-9-]*", "Start with a letter; letters, digits and - only")
        .max_len(64)
        .hint("Shown in the sidebar"),
    FormField::new("limit", "Rows", FieldKind::Number { min: Some(1.0), max: Some(100.0) }),
    FormField::new("format", "Format", FieldKind::Select(Choices::Static(&[("csv", "CSV")]))),
    FormField::new("owner", "Owner", FieldKind::Select(Choices::Endpoint {
        source: Source::get("admin", "/users"), value: &["id"], label: &["name"] })),
    FormField::new("notes", "Notes", FieldKind::Multiline),
    FormField::new("public", "Public", FieldKind::Checkbox),
]};
const RENAME_ACTION: ActionSpec = ActionSpec::new('n', "Rename", ActionKind::Form(&RENAME, RequestSpec {
    source: Source::new("x", Verb::Patch, "/thing/{Id}", Some(r#"{"Name":"{name}","limit":{limit}}"#)),
    success: "Renamed", after: After::Refresh,
}));
```

Submitted values are JSON: text is a string, a number is a number (put it
unquoted in the body), a checkbox is a bool, a select is the chosen value.
Keys: Tab and Shift-Tab move between fields, Enter submits (Ctrl+S in a
multiline field), ←/→ or space change a select, space toggles a checkbox,
Esc cancels.

## Add a link

```rust
links: &[LinkSpec {
    key: 'L', label: "lineage",
    goto: Goto {
        target: Target { module: "Catalog", resource: "Data lineage" },
        select: Select::Drill { field: &["Id"], from: &["Id"] }, // or Row / Filter / Open
    },
    when: None,
}],
```

The target opens at its root. Once its rows load, the row whose `field`
equals this row's `from` is selected (`Row`), selected and entered (`Drill`),
or the list is filtered by it (`Filter`). Back (← or Esc at the root)
returns to where the link was followed from. A test makes sure every target
resolves.

## Add a full-screen view

One file under `views/`, one type implementing `View`:

```rust
pub struct RunLog { run_id: String, poller: Poller, lines: Vec<String>, waiting: Option<u64> }

impl View for RunLog {
    fn title(&self) -> String { format!("Run {}", self.run_id) }
    fn on_open(&mut self, ctx: &mut Ctx) { self.fetch(ctx) }
    fn on_key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled { Handled::No } // Esc/q pop, r → refresh, ? → help
    fn on_fetched(&mut self, id: u64, result: Result<Value, String>, ctx: &mut Ctx) { … }
    fn on_tick(&mut self, ctx: &mut Ctx) {
        if self.poller.due(ctx.now.instant) { self.fetch(ctx) }    // stops when popped
    }
    fn refresh(&mut self, ctx: &mut Ctx) { self.fetch(ctx) }
    fn render(&mut self, frame: &mut Frame, area: Rect, ui: RenderCtx) { … }
    fn help(&self) -> Vec<HelpSection> { vec![HelpSection::new("Run log", &[("f", "follow")])] }
    fn hints(&self) -> Vec<(&'static str, &'static str)> { vec![("f", "follow"), ("esc", "back")] }
}
```

- Fetch with `ctx.fetch(&source, &scope)` or `ctx.fetch_raw(...)`. The answer
  comes back to this view's `on_fetched` with the returned id. If the view
  was popped, the answer is dropped. `ctx.run_task(…)` does the same for
  async code with the API client (`views/sql.rs` drives `gz sql`'s
  `Lakehouse` client this way).
- `views/text_table.rs` draws tables whose columns are only known at run
  time (sample rows, a schema).
- Follow a run's server-sent events with `ctx.stream(service, path)`: each
  event, then `StreamItem::Ended` (or `Failed`), comes to `on_stream` with
  the returned id; reconnects are handled. `ctx.stop_stream(id)` stops
  reading (see `views/playground.rs`).
- Open a view from a row with a custom action
  (`ActionKind::Custom(|row, ctx| ctx.push(Box::new(RunLog::new(row))))`),
  from another view with `ctx.push(...)`, or from a notification with
  `Jump::View(Rc::new(|| Box::new(…)))`.
- Talk to the app only through `Ctx`: `toast`, `notify`, `copy`, `push`,
  `pop`, and `effect(Effect::RunAction | ActionMenu | Follow | Open)`.
  `ctx.now` is the clock: use it, never `Instant::now()`, so tests can step
  time with `Now::advanced`.
- Draw with `theme.rs` styles and the `ui::widgets` helpers (`fit`,
  `key_hints`, `render_scrollbar`, `highlight_json`, `centered`).
- Notifications:
  `ctx.notify(Notification::new(BadgeTone::Good, "Run 42 finished").jump_to(Jump::Resource(id)))`.
  The notice shows in the status bar for 6 s and `ctrl+g` opens its target.

## Add help entries

- Screen: actions and links are listed automatically. Add other keys the
  screen handles with `keys: &[KeyHelp { keys: "s", what: "browse tables" }]`.
- View: return them from `View::help()`. They are listed before the global
  keys.
- Global keys: `help::global()` ("Views" and "Global").
- The overlay groups sections as This screen, Actions, Views, Global; `/`
  filters entries as you type, and it scrolls when it doesn't fit.

## Errors and empty states

Never format an error by hand. Use
`errors::explain(&err, Subject { what: "this run", service: "agents" })`. It
returns "You don't have access to this run." for 403, and for a 401 that is
really a permission check (the server's message mentions permissions, or the
service is `agents` or `mlops`, which answer `AccessDenied` with 401). Other
401s say "Your sign-in has expired…". It returns "This run was deleted or never existed." for 404, "The agents
service had an error." for 5xx, and "Can't reach the agents service…"
offline, with the server's message added in brackets.
`errors::is_access_denied(&err, service)` tells a permission denial apart
(a tile or notice can then stay quiet instead of showing an error).

Set a screen's empty text with `empty`. Draw loading, empty and failed
states with `ui::states` (`loading`, `empty`, `failed`) so they look the
same in tables, tiles and views.

## Home dashboard and notices

- `H` (and every launch, unless you last left from another screen) opens
  `views/home/`: failed job runs in the last 24 h, running jobs (rows
  "running" for over 24 h fold into one "may be stuck" line), agent
  versions waiting for your review, this month's AI spend and recent chats.
  Tiles parse their answers in `tiles.rs` (pure, tested); each item drills
  with a `Goto` (`TARGETS` is checked by a test) or `Effect::Command`.
- Tiles are as tall as their content. `render.rs` starts every list
  (failed runs, approvals, running, chats) as a one-line summary and hands
  each spare row to the list with the most items still hidden; Up/Down
  reach exactly the items shown. An empty tile is one line.
- A detail view names its row once inside: the header line (name and
  status). The breadcrumb says where it is, and the frame title only says
  `Details` or `Raw JSON`.
- Spend lines stay separate on purpose: your chat spend, the whole site's
  spend (which already includes agent-builder runs), and the rolling 30-day
  policy budget are different windows and scopes.
- Never call `GET /etl/jobs/{id}/current-run` or `POST /etl/logs` from a
  tile or poller: they can mark runs Failed.
- `watch.rs` looks 10 s after launch (learning what exists) and then every
  3 minutes; it raises one notice per kind of news, with a count when
  several arrive. Notices can jump with `Jump::Follow(goto, row)`.

## Remembered UI state

`ui_state.rs` saves, on quit, the place on screen, whether Home was in
front, and each visited table's sort and column scroll to `tui_state.json`
next to the gz config. Places are stored by sidebar titles. A missing or
corrupt file is a fresh start. Saved views (`tui_views.json`) are separate.

## Testing

Use `app::test_support` from any test in the crate:

- `app()` builds an app with the clock fixed at `TEST_NOW_MS`
  (2026-11-08T12:00Z), so "2h ago" is stable.
- `press(&mut app, &[KeyCode::…])` sends keys.
- `last_fetch(&app)` returns the request the app queued (nothing is sent),
  leaving out the background news check.
- `answer(&mut app, json!(…))` answers that request.
- `select_row(&mut app, &["Name"], "GZS3")` selects a row.
- `draw(&mut app, w, h)` renders to a string through `TestBackend`.
- `resource("Module", "Label")` looks up a resource.

- `answer_path(&mut app, path, json!(…))` answers one of several requests
  sent together (a composed level); `last_task(&app)` returns the newest
  task (answer it with `app.fetched(task.id, task.target, …)`).

To check a real request's wire shape, run it through `fetch::Executor`
against a local stub server, or call the task's async function with
`test_stub::serve` and `test_stub::client` (see `views/sql/tests.rs`). `specs/connections/tests.rs`
(`test_connection_request_reaches_the_lakehouse_route`) shows how. Never
call the real backend from tests.

## Navigation

| Key | What |
|---|---|
| `/` | Global search (`views/search.rs`): screen titles, then `GET chat /search` hits grouped by kind; Tab narrows to one kind; Enter opens the hit selected in its screen. |
| `ctrl+f` | Filter the table: every word must appear in the row's shown cells. Esc clears. |
| `o` / `O` | Sort by the next column / reverse. Raw values compare as numbers, times, sizes. |
| `ctrl+k` | Palette: Recent, saved views, "Save this view…", screens, actions, search. |
| `ctrl+e` | Recent screens and items only. |
| `⌫` `esc` `←` | Back: up a drill level, then through the history of links, jumps and saved views. |

- A `Place` (`app/history.rs`) is a resource plus `Step`s (select a row by
  field, or drill into it), a filter and a sort. `App::go_to(&place)`
  pushes the current place on the Back stack and walks the steps as rows
  arrive. Views ask for it with `Effect::Go(place, label)`.
- Saved views (`saved_views.rs`) store a `Place` by sidebar titles in
  `tui_views.json` next to the gz config (owner-only).
- A search kind opens where `KINDS` in `views/search.rs` says. If you rename
  a screen a kind points at, `every_kind_opens_a_screen_that_exists` fails.

## Notes

- Some vocabulary items carry
  `#[allow(dead_code, reason = "spec vocabulary …")]` because no converted
  screen uses them yet. Remove the allow when you start using the item.
  Once every screen is converted, the allows should all be gone.
- `nav::ID_KEYS` and positional `{0}` templates remain for unconverted
  screens. Converted screens should use `Identity` and named fields.
