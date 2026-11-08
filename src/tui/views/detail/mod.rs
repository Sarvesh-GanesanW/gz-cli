//! A row's detail view: header with title, status badge and subtitle,
//! labelled sections, related tables loaded in the background, and the raw
//! JSON one key away. Built entirely from the screen's `DetailSpec`.

mod lines;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Alignment, Rect},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Padding, Paragraph},
    Frame,
};
use serde_json::Value;

use super::{Ctx, Effect, Handled, Poller, RenderCtx, RowContext, View};
use crate::output;
use crate::tui::errors::{self, Subject};
use crate::tui::fields;
use crate::tui::format::{self, Shown};
use crate::tui::help::HelpSection;
use crate::tui::screen::{allowed, DetailSpec, RelatedSpec, ScreenSpec};
use crate::tui::template::{self, Encoding};
use crate::tui::theme;
use crate::tui::ui::widgets::render_scrollbar;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Load {
    Ready,
    Loading(u64),
    /// The detail endpoint answered with nothing.
    Missing,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RelatedState {
    /// Not loaded for this row (its `when` doesn't hold), or not yet.
    Skipped,
    Loading(u64),
    Rows(Vec<Vec<Shown>>),
    Failed(String),
    /// The user may not see it (403): leave the block out.
    Hidden,
}

#[derive(Debug, Clone)]
struct Related {
    spec: &'static RelatedSpec,
    state: RelatedState,
}

pub struct DetailView {
    spec: &'static ScreenSpec,
    detail: &'static DetailSpec,
    row: RowContext,
    load: Load,
    related: Vec<Related>,
    raw: bool,
    scroll: usize,
    viewport: usize,
    line_count: usize,
    poller: Option<Poller>,
}

impl DetailView {
    /// None when the screen has no detail spec.
    pub fn new(spec: &'static ScreenSpec, row: RowContext) -> Option<Self> {
        let detail = spec.detail.as_ref()?;
        Some(Self {
            spec,
            detail,
            row,
            load: Load::Ready,
            related: detail
                .related
                .iter()
                .map(|spec| Related {
                    spec,
                    state: RelatedState::Skipped,
                })
                .collect(),
            raw: false,
            scroll: 0,
            viewport: 1,
            line_count: 0,
            poller: detail.poll.map(Poller::every),
        })
    }

    fn explain(&self, err: &str, service: &str) -> String {
        let what = format!("this {}", self.spec.noun);
        errors::explain(
            err,
            Subject {
                what: &what,
                service,
            },
        )
    }

    /// Fetches the details, or (without a detail endpoint) the related blocks.
    fn start(&mut self, ctx: &mut Ctx) {
        let Some(source) = self.detail.load else {
            self.load = Load::Ready;
            return self.start_related(ctx);
        };
        let scope = self.row.scope(None, ctx.site.as_deref());
        self.load = match ctx.fetch(&source, &scope) {
            Ok(id) => Load::Loading(id),
            Err(err) => Load::Failed(format!("Can't load details: {err}")),
        };
    }

    fn start_related(&mut self, ctx: &mut Ctx) {
        let site = ctx.site.clone();
        for index in 0..self.related.len() {
            let spec = self.related[index].spec;
            let state = if !allowed(spec.when, &self.row.row) {
                RelatedState::Skipped
            } else {
                let scope = self.row.scope(None, site.as_deref());
                match ctx.fetch(&spec.source, &scope) {
                    Ok(id) => RelatedState::Loading(id),
                    Err(err) => RelatedState::Failed(err.to_string()),
                }
            };
            self.related[index].state = state;
        }
    }

    fn details_loaded(&mut self, result: Result<Value, String>, ctx: &mut Ctx) {
        match result {
            Ok(answer) => match first_object(&answer) {
                Some(object) => {
                    let merged = fields::merged(&self.row.row, &object);
                    self.row.row = self.spec.enriched(&merged, ctx.now.epoch_ms);
                    self.load = Load::Ready;
                    self.start_related(ctx);
                }
                None => self.load = Load::Missing,
            },
            Err(err) => {
                let service = self.detail.load.map_or("", |s| s.service);
                self.load = Load::Failed(self.explain(&err, service));
            }
        }
    }

    fn related_loaded(&mut self, index: usize, result: Result<Value, String>, now_ms: i64) {
        let spec = self.related[index].spec;
        self.related[index].state = match result {
            Ok(answer) => {
                // A source that answers one object (a run's governance
                // record) shows as a single row.
                let rows = match output::tabular(&answer) {
                    Value::Array(items) => items.as_slice(),
                    object @ Value::Object(_) => std::slice::from_ref(object),
                    _ => &[],
                };
                RelatedState::Rows(
                    rows.iter()
                        .take(spec.limit)
                        .map(|row| {
                            spec.columns
                                .iter()
                                .map(|c| {
                                    format::show(fields::lookup_any(row, c.field), c.format, now_ms)
                                })
                                .collect()
                        })
                        .collect(),
                )
            }
            // The agent builder says "no access" with a 401 `AccessDenied`.
            Err(err) if errors::is_access_denied(&err, spec.source.service) => RelatedState::Hidden,
            // Optional records (a run's observability) answer 404 when they
            // were never written: that is the block's empty state.
            Err(err)
                if matches!(
                    errors::parse(&err),
                    errors::Failure::Http { status: 404, .. }
                ) =>
            {
                RelatedState::Rows(Vec::new())
            }
            Err(err) => RelatedState::Failed(self.explain(&err, spec.source.service)),
        };
    }

    fn scroll_by(&mut self, delta: isize) {
        let max = self.line_count.saturating_sub(self.viewport) as isize;
        self.scroll = (self.scroll as isize + delta).clamp(0, max.max(0)) as usize;
    }

    fn copy_alt(&self, ctx: &mut Ctx) {
        let Some(copy) = self.detail.copy else {
            return ctx.toast(crate::tui::app::Tone::Error, "Nothing else to copy here");
        };
        let scope = self.row.scope(None, ctx.site.as_deref());
        match template::fill(copy.template, &scope, Encoding::Text) {
            Ok(text) => ctx.copy(text, copy.label),
            Err(err) => ctx.toast(crate::tui::app::Tone::Error, format!("Can't copy: {err}")),
        }
    }
}

/// The detail object in an answer: unwrapped from its envelope, and the
/// first element when the endpoint returns a one-item list.
fn first_object(answer: &Value) -> Option<Value> {
    match output::tabular(answer) {
        Value::Array(items) => items.first().cloned(),
        Value::Null => None,
        other => Some(other.clone()),
    }
}

impl View for DetailView {
    fn title(&self) -> String {
        self.spec
            .identity
            .title_of(&self.row.row)
            .unwrap_or_else(|| self.spec.noun.to_string())
    }

    fn on_open(&mut self, ctx: &mut Ctx) {
        // Actions started here refresh (or close) this view when they finish.
        self.row.view = ctx.view_id();
        self.start(ctx);
    }

    fn on_key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled {
        let page = self.viewport.max(1) as isize;
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.scroll_by(-1),
            KeyCode::Down | KeyCode::Char('j') => self.scroll_by(1),
            KeyCode::PageUp => self.scroll_by(-page),
            KeyCode::PageDown | KeyCode::Char(' ') => self.scroll_by(page),
            KeyCode::Home | KeyCode::Char('g') => self.scroll = 0,
            KeyCode::End | KeyCode::Char('G') => self.scroll_by(isize::MAX / 2),
            KeyCode::Char('J') => {
                self.raw = !self.raw;
                self.scroll = 0;
            }
            KeyCode::Char('y') => match self.spec.identity.id_of(&self.row.row) {
                Some(id) => ctx.copy(id.clone(), format!("{} id {id}", self.spec.noun)),
                None => ctx.toast(crate::tui::app::Tone::Error, "This row has no id"),
            },
            KeyCode::Char('Y') => self.copy_alt(ctx),
            KeyCode::Char('a') => ctx.effect(Effect::ActionMenu(self.row.clone())),
            KeyCode::Char(c) => {
                let row = &self.row.row;
                if let Some(action) = self.spec.actions_for(row).find(|a| a.key == c) {
                    ctx.effect(Effect::RunAction(action, self.row.clone()));
                } else if let Some(link) = self.spec.links_for(row).find(|l| l.key == c) {
                    ctx.effect(Effect::Follow(link.goto, self.row.clone()));
                } else {
                    return Handled::No;
                }
            }
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn on_fetched(&mut self, id: u64, result: Result<Value, String>, ctx: &mut Ctx) {
        if self.load == Load::Loading(id) {
            return self.details_loaded(result, ctx);
        }
        if let Some(index) = self
            .related
            .iter()
            .position(|r| r.state == RelatedState::Loading(id))
        {
            self.related_loaded(index, result, ctx.now.epoch_ms);
        }
    }

    fn on_tick(&mut self, ctx: &mut Ctx) {
        let due = self.poller.as_mut().is_some_and(|p| p.due(ctx.now.instant));
        if due && !self.is_busy() {
            self.start(ctx);
        }
    }

    fn refresh(&mut self, ctx: &mut Ctx) {
        self.start(ctx);
    }

    fn on_scroll(&mut self, delta: isize) {
        self.scroll_by(delta);
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, ui: RenderCtx) {
        let hint = if self.raw {
            " J formatted "
        } else {
            " J raw JSON "
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(theme::border(true))
            .title(Span::styled(format!(" {} ", self.title()), theme::title()))
            .title(Line::from(Span::styled(hint, theme::muted())).alignment(Alignment::Right))
            .padding(Padding::horizontal(1));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let lines = self.lines(ui, inner.width as usize);
        self.viewport = inner.height as usize;
        self.line_count = lines.len();
        self.scroll_by(0);
        let shown: Vec<Line> = lines
            .into_iter()
            .skip(self.scroll)
            .take(self.viewport)
            .collect();
        frame.render_widget(Paragraph::new(shown), inner);
        if self.line_count > self.viewport {
            render_scrollbar(
                frame,
                area,
                self.line_count.saturating_sub(self.viewport),
                self.scroll,
            );
        }
    }

    fn help(&self) -> Vec<HelpSection> {
        let mut keys = HelpSection::new(
            "Details",
            &[
                ("↑ ↓  PgUp PgDn", "scroll"),
                ("J", "raw JSON ↔ formatted"),
                ("y", "copy the id"),
                ("a", "actions"),
                ("r", "reload"),
                ("esc", "back"),
            ],
        );
        if let Some(copy) = self.detail.copy {
            keys.entries
                .push(("Y".into(), format!("copy the {}", copy.label)));
        }
        let screen = crate::tui::help::for_screen("This row", self.spec, Some(&self.row.row));
        vec![keys, screen]
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        vec![
            ("↑↓", "scroll"),
            ("J", "raw"),
            ("y", "copy id"),
            ("a", "actions"),
            ("esc", "back"),
        ]
    }

    fn is_busy(&self) -> bool {
        matches!(self.load, Load::Loading(_))
            || self
                .related
                .iter()
                .any(|r| matches!(r.state, RelatedState::Loading(_)))
    }
}

#[cfg(test)]
mod tests;
