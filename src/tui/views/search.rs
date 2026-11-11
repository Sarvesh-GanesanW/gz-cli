//! Global search (`/` anywhere): finds screens by title and, as you type,
//! every resource kind the server indexes (`GET chat /search`, the web
//! app's Cmd-K finder). Results are grouped by kind; Tab narrows to one
//! kind; Enter opens the screen the hit lives on with the hit selected.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    layout::{Alignment, Constraint, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Padding, Paragraph, Wrap},
    Frame,
};
use reqwest::Method;
use serde_json::Value;

use super::{Ctx, Effect, Handled, RenderCtx, View};
use crate::cmd::search::group_by_kind;
use crate::tui::app::{Place, Step, Tone};
use crate::tui::errors::{self, Subject};
use crate::tui::fields::{self, FieldPath};
use crate::tui::format;
use crate::tui::help::HelpSection;
use crate::tui::nav::{self, all_resources, ResourceId};
use crate::tui::screen::Target;
use crate::tui::theme;
use crate::tui::ui::widgets::{fit, render_scrollbar};

/// Typing pauses this long before a search is sent (the web waits 120 ms).
const DEBOUNCE: Duration = Duration::from_millis(150);
/// The server's maximum page.
const RESULT_LIMIT: usize = 50;
/// The server rejects longer queries.
const MAX_QUERY_CHARS: usize = 200;
/// Screens matched by title, shown above the server's hits.
const MAX_SCREENS: usize = 5;

/// How a hit of one kind is found in its screen.
#[derive(Debug, Clone, Copy)]
enum Route {
    /// Select the root row whose `field` equals the hit's `from`.
    Row {
        field: FieldPath,
        from: &'static str,
    },
    /// Drill into the parent row (`parent` = the hit's `parentId`), then
    /// select the row whose `field` equals the hit's `id`.
    InParent { parent: FieldPath, field: FieldPath },
}

/// One of the server's search kinds and where its hits open.
#[derive(Debug, Clone, Copy)]
struct Kind {
    id: &'static str,
    plural: &'static str,
    target: Target,
    route: Route,
}

const fn kind(id: &'static str, plural: &'static str, target: Target, route: Route) -> Kind {
    Kind {
        id,
        plural,
        target,
        route,
    }
}

const fn target(module: &'static str, resource: &'static str) -> Target {
    Target { module, resource }
}

const BY_ID: Route = Route::Row {
    field: &["Id", "id"],
    from: "id",
};

/// The server's 12 kinds (gz-chat `routes/search.py`), mapped to screens as
/// the web finder routes them (`finderKinds.js`).
static KINDS: &[Kind] = &[
    kind("dataset", "Datasets", target("Designer", "Datasets"), BY_ID),
    kind("filter", "Filters", target("Designer", "Filters"), BY_ID),
    kind(
        "visualization",
        "Visualizations",
        target("Designer", "Visualizations"),
        BY_ID,
    ),
    kind(
        "dashboard",
        "Dashboards",
        target("Designer", "Dashboards"),
        BY_ID,
    ),
    kind(
        "connection",
        "Connections",
        target("Connections", "Connections"),
        BY_ID,
    ),
    kind(
        "workspace",
        "Workspaces",
        target("Workspaces", "Workspaces"),
        BY_ID,
    ),
    kind("project", "Projects", target("DE/ML", "Projects"), BY_ID),
    kind(
        "job",
        "Jobs",
        target("DE/ML", "Projects"),
        Route::InParent {
            parent: &["Id", "id", "projectId"],
            field: &["Id", "id", "jobId"],
        },
    ),
    // An agent's numeric id isn't the API's; its routeKey (a UUID) is.
    kind(
        "agent",
        "Agents",
        target("Agents", "Agents"),
        Route::Row {
            field: &["id"],
            from: "routeKey",
        },
    ),
    // Warehouse drills go by name, so rows are matched by name too.
    kind(
        "warehouse",
        "Warehouses",
        target("Lakehouse", "Warehouses"),
        Route::Row {
            field: &["name"],
            from: "name",
        },
    ),
    kind(
        "savedQuery",
        "Saved queries",
        target("Lakehouse", "Saved queries"),
        BY_ID,
    ),
    kind(
        "ontologyModel",
        "Ontology models",
        target("Catalog", "Ontology models"),
        BY_ID,
    ),
];

fn kind_of(id: &str) -> Option<&'static Kind> {
    KINDS.iter().find(|k| k.id == id)
}

/// Text of a hit's field: strings and numbers only.
fn hit_text(hit: &Value, field: &str) -> Option<String> {
    fields::lookup(hit, field).and_then(fields::id_text)
}

/// Where a hit opens: its screen and the steps to its row.
fn place_for(hit: &Value) -> Result<Place, String> {
    let kind_id = hit["kind"].as_str().unwrap_or_default();
    let kind = kind_of(kind_id).ok_or_else(|| format!("gz can't open {kind_id} results yet"))?;
    let Target { module, resource } = kind.target;
    let resource = nav::find_resource(module, resource)
        .ok_or_else(|| format!("{module} › {resource} no longer exists"))?;
    let missing = |field: &str| format!("This result has no {field}");
    let steps = match kind.route {
        Route::Row { field, from } => {
            vec![Step::select(
                field,
                hit_text(hit, from).ok_or_else(|| missing(from))?,
            )]
        }
        Route::InParent { parent, field } => vec![
            Step::drill(
                parent,
                hit_text(hit, "parentId").ok_or_else(|| missing("parentId"))?,
            ),
            Step::select(field, hit_text(hit, "id").ok_or_else(|| missing("id"))?),
        ],
    };
    Ok(Place {
        steps,
        ..Place::at(resource)
    })
}

/// One line of results.
#[derive(Debug, Clone, PartialEq)]
enum Hit {
    Screen(ResourceId),
    Item(Value),
}

#[derive(Debug, Clone, PartialEq)]
struct Group {
    title: String,
    hits: Vec<Hit>,
}

/// Screens whose title holds the query, best (shortest) first.
fn matching_screens(query: &str) -> Vec<Hit> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return Vec::new();
    }
    let mut screens: Vec<ResourceId> = all_resources()
        .filter(|id| id.title().to_lowercase().contains(&needle))
        .collect();
    screens.sort_by_key(|id| id.title().len());
    screens.truncate(MAX_SCREENS);
    screens.into_iter().map(Hit::Screen).collect()
}

/// The server's hits grouped by kind, kinds in order of their best hit.
fn item_groups(results: &[Value]) -> Vec<Group> {
    group_by_kind(results)
        .into_iter()
        .map(|(kind, hits)| Group {
            title: kind_of(&kind).map_or(kind, |k| k.plural.to_string()),
            hits: hits.into_iter().cloned().map(Hit::Item).collect(),
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq)]
enum State {
    Idle,
    Waiting,
    Loading(u64),
    Done,
    Failed(String),
}

pub struct SearchView {
    query: String,
    /// Index into [`KINDS`] when narrowed to one kind.
    kind: Option<usize>,
    groups: Vec<Group>,
    selected: usize,
    state: State,
    /// When the query or kind last changed; a search goes out after the
    /// debounce.
    edited_at: Option<Instant>,
    scroll: usize,
    viewport: usize,
}

impl SearchView {
    pub fn new(query: &str) -> Self {
        let mut view = Self {
            query: query.chars().take(MAX_QUERY_CHARS).collect(),
            kind: None,
            groups: Vec::new(),
            selected: 0,
            state: State::Idle,
            edited_at: None,
            scroll: 0,
            viewport: 10,
        };
        view.groups = view.screen_groups();
        view
    }

    fn screen_groups(&self) -> Vec<Group> {
        let screens = matching_screens(&self.query);
        if screens.is_empty() || self.kind.is_some() {
            return Vec::new();
        }
        vec![Group {
            title: "Screens".to_string(),
            hits: screens,
        }]
    }

    fn hits(&self) -> impl Iterator<Item = &Hit> {
        self.groups.iter().flat_map(|g| g.hits.iter())
    }

    fn hit_count(&self) -> usize {
        self.groups.iter().map(|g| g.hits.len()).sum()
    }

    fn edited(&mut self, now: Instant) {
        self.groups = self.screen_groups();
        self.selected = 0;
        self.scroll = 0;
        self.edited_at = Some(now);
        self.state = if self.query.trim().is_empty() {
            State::Idle
        } else {
            State::Waiting
        };
    }

    /// `/search?q=…&limit=50[&kinds=…]`.
    fn search_path(&self) -> String {
        let mut query = url::form_urlencoded::Serializer::new(String::new());
        query.append_pair("q", self.query.trim());
        query.append_pair("limit", &RESULT_LIMIT.to_string());
        if let Some(kind) = self.kind {
            query.append_pair("kinds", KINDS[kind].id);
        }
        format!("/search?{}", query.finish())
    }

    fn search(&mut self, ctx: &mut Ctx) {
        self.edited_at = None;
        if self.query.trim().is_empty() {
            self.state = State::Idle;
            return;
        }
        let id = ctx.fetch_raw("chat", Method::GET, self.search_path(), None);
        self.state = State::Loading(id);
    }

    fn cycle_kind(&mut self, forward: bool, now: Instant) {
        let count = KINDS.len() + 1;
        let position = self.kind.map_or(0, |k| k + 1);
        let next = if forward {
            (position + 1) % count
        } else {
            (position + count - 1) % count
        };
        self.kind = next.checked_sub(1);
        self.edited(now);
    }

    fn move_sel(&mut self, delta: isize) {
        let last = self.hit_count().saturating_sub(1) as isize;
        self.selected = (self.selected as isize + delta).clamp(0, last.max(0)) as usize;
    }

    fn open_selected(&self, ctx: &mut Ctx) {
        match self.hits().nth(self.selected) {
            Some(Hit::Screen(resource)) => {
                ctx.effect(Effect::Go(Place::at(*resource), resource.title()))
            }
            Some(Hit::Item(hit)) => match place_for(hit) {
                Ok(place) => {
                    let name = hit_text(hit, "name").unwrap_or_default();
                    let label = format!("{} › {name}", place.resource.title());
                    ctx.effect(Effect::Go(place, label));
                }
                Err(message) => ctx.toast(Tone::Error, message),
            },
            None => {}
        }
    }

    fn kind_chips(&self) -> Line<'static> {
        let mut spans = vec![Span::styled("kind ", theme::muted())];
        let names = std::iter::once("All").chain(KINDS.iter().map(|k| k.plural));
        for (position, name) in names.enumerate() {
            let chosen = self.kind.map_or(0, |k| k + 1) == position;
            let style = if chosen {
                theme::selected_row()
            } else {
                theme::muted()
            };
            spans.push(Span::styled(format!(" {name} "), style));
        }
        Line::from(spans)
    }

    /// The results as lines, and the line of each hit in order.
    fn result_lines(&self, width: usize, now_ms: i64) -> (Vec<Line<'static>>, Vec<usize>) {
        let mut lines = Vec::new();
        let mut hit_lines = Vec::new();
        let mut index = 0;
        let name_width = self
            .hits()
            .map(|hit| hit_name(hit).chars().count())
            .max()
            .unwrap_or(0)
            .min(width / 2);
        for group in &self.groups {
            if !lines.is_empty() {
                lines.push(Line::from(""));
            }
            lines.push(Line::from(Span::styled(
                format!("{} ({})", group.title, group.hits.len()),
                theme::title(),
            )));
            for hit in &group.hits {
                hit_lines.push(lines.len());
                let selected = index == self.selected;
                lines.push(hit_line(hit, selected, (name_width, width), now_ms));
                index += 1;
            }
        }
        (lines, hit_lines)
    }

    fn placeholder(&self) -> Option<Line<'static>> {
        let text = match &self.state {
            State::Idle if self.groups.is_empty() => {
                "Start typing to find screens and resources.".to_string()
            }
            State::Failed(message) => message.clone(),
            State::Done if self.groups.is_empty() => "No matching resources found.".to_string(),
            _ => return None,
        };
        let style = match self.state {
            State::Failed(_) => Style::default().fg(theme::ERROR),
            _ => theme::muted(),
        };
        Some(Line::from(Span::styled(text, style)))
    }
}

fn hit_name(hit: &Hit) -> String {
    match hit {
        Hit::Screen(resource) => resource.title(),
        Hit::Item(hit) => hit_text(hit, "name").unwrap_or_else(|| "(unnamed)".into()),
    }
}

/// `▸ name   context · owner · 2h ago`, names padded to `name_width`.
fn hit_line(
    hit: &Hit,
    selected: bool,
    (name_width, width): (usize, usize),
    now_ms: i64,
) -> Line<'static> {
    let details = match hit {
        Hit::Screen(_) => "screen".to_string(),
        Hit::Item(hit) => {
            let mut details: Vec<String> = ["context", "owner"]
                .iter()
                .filter_map(|k| hit[*k].as_str().filter(|s| !s.is_empty()))
                .map(str::to_string)
                .collect();
            if let Some(at) = format::epoch_ms(&hit["updatedOn"]) {
                details.push(format::relative_time(at, now_ms));
            }
            details.join(" · ")
        }
    };
    let marker = if selected { "▸ " } else { "  " };
    let name = format!("{:<name_width$}", fit(&hit_name(hit), name_width));
    let details = fit(&details, width.saturating_sub(name_width + 5));
    let style = if selected {
        theme::selected_row()
    } else {
        Style::default()
    };
    Line::from(vec![
        Span::styled(format!("{marker}{name}"), style),
        Span::raw("   "),
        Span::styled(details, theme::muted()),
    ])
}

impl View for SearchView {
    fn title(&self) -> String {
        "Search".to_string()
    }

    // It searches everything, so the screen it was opened from is no part
    // of where the user is.
    fn stands_alone(&self) -> bool {
        true
    }

    fn on_open(&mut self, ctx: &mut Ctx) {
        if !self.query.trim().is_empty() {
            self.search(ctx);
        }
    }

    fn on_key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled {
        let now = ctx.now.instant;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('u') if ctrl => {
                self.query.clear();
                self.edited(now);
            }
            KeyCode::Char(c) if !ctrl => {
                if self.query.chars().count() < MAX_QUERY_CHARS {
                    self.query.push(c);
                    self.edited(now);
                }
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.edited(now);
            }
            KeyCode::Tab => self.cycle_kind(true, now),
            KeyCode::BackTab => self.cycle_kind(false, now),
            KeyCode::Up => self.move_sel(-1),
            KeyCode::Down => self.move_sel(1),
            KeyCode::PageUp => self.move_sel(-(self.viewport as isize)),
            KeyCode::PageDown => self.move_sel(self.viewport as isize),
            KeyCode::Enter => self.open_selected(ctx),
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn on_fetched(&mut self, id: u64, result: Result<Value, String>, _ctx: &mut Ctx) {
        // Answers to superseded queries are dropped.
        if self.state != State::Loading(id) {
            return;
        }
        match result {
            Ok(value) => {
                let results = value["results"].as_array().cloned().unwrap_or_default();
                let mut groups = self.screen_groups();
                groups.extend(item_groups(&results));
                self.groups = groups;
                self.move_sel(0);
                self.state = State::Done;
            }
            Err(err) => {
                let subject = Subject {
                    what: "search",
                    service: "chat",
                };
                self.state = State::Failed(errors::explain(&err, subject));
            }
        }
    }

    fn on_tick(&mut self, ctx: &mut Ctx) {
        let due = self
            .edited_at
            .is_some_and(|at| ctx.now.instant.duration_since(at) >= DEBOUNCE);
        if due {
            self.search(ctx);
        }
    }

    fn refresh(&mut self, ctx: &mut Ctx) {
        self.search(ctx);
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, ui: RenderCtx) {
        let count = match self.state {
            State::Loading(_) | State::Waiting => " searching… ".to_string(),
            _ if self.groups.is_empty() => String::new(),
            _ => format!(" {} result(s) ", self.hit_count()),
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(theme::border(true))
            .title(Span::styled(" Search ", theme::title()))
            .title(Line::from(Span::styled(count, theme::muted())).alignment(Alignment::Right))
            .padding(Padding::horizontal(1));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let [input, chips, list] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Length(3),
            Constraint::Min(1),
        ])
        .areas(inner);
        let prompt = Line::from(vec![
            Span::styled("/ ", theme::key_hint()),
            Span::raw(self.query.clone()),
            Span::styled("▏", Style::default().fg(theme::ACCENT)),
        ]);
        frame.render_widget(Paragraph::new(prompt), input);
        frame.render_widget(
            Paragraph::new(self.kind_chips()).wrap(Wrap { trim: true }),
            chips,
        );
        if let Some(line) = self.placeholder() {
            frame.render_widget(Paragraph::new(line).wrap(Wrap { trim: true }), list);
            return;
        }
        let (lines, hit_lines) = self.result_lines(list.width as usize, ui.now_ms);
        self.viewport = list.height.max(1) as usize;
        let at = hit_lines.get(self.selected).copied().unwrap_or(0);
        if at >= self.scroll + self.viewport {
            self.scroll = at + 1 - self.viewport;
        }
        // Keep the group title above the first hit in view.
        if at <= self.scroll {
            self.scroll = at.saturating_sub(1);
        }
        let total = lines.len();
        let shown: Vec<Line> = lines
            .into_iter()
            .skip(self.scroll)
            .take(self.viewport)
            .collect();
        frame.render_widget(Paragraph::new(shown), list);
        if total > self.viewport {
            render_scrollbar(
                frame,
                area,
                total.saturating_sub(self.viewport),
                self.scroll,
            );
        }
    }

    fn help(&self) -> Vec<HelpSection> {
        vec![HelpSection::new(
            "Search",
            &[
                ("type", "search screens and resources"),
                ("↑ ↓  PgUp PgDn", "pick a result"),
                ("tab  shift+tab", "only one kind / all kinds"),
                ("⏎", "open it, selected in its screen"),
                ("ctrl+u", "clear the query"),
                ("esc", "close"),
            ],
        )]
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        vec![
            ("⏎", "open"),
            ("tab", "kind"),
            ("↑↓", "pick"),
            ("esc", "close"),
        ]
    }

    fn is_busy(&self) -> bool {
        matches!(self.state, State::Loading(_))
    }

    fn is_typing(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests;
