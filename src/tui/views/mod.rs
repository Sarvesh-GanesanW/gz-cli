//! Full-screen views stacked over the browser: a row's detail, and later a
//! dashboard, run viewer, lineage graph, search, usage charts…
//!
//! A view is one type implementing [`View`] in its own file. It gets keys,
//! fetch answers and clock ticks, and talks back to the app only through a
//! [`Ctx`]: fetching, pushing or popping views, toasts, notifications,
//! copying and running row actions. The app owns the [`ViewStack`]; Esc
//! pops the top view unless the view handles it.

pub mod detail;
mod poll;
pub mod search;

pub use poll::Poller;

use crossterm::event::KeyEvent;
use ratatui::{layout::Rect, Frame};
use reqwest::Method;
use serde_json::Value;

use crate::tui::app::{Place, Tone};
use crate::tui::clock::Now;
use crate::tui::fetch::{FetchReq, FetchTarget, Request};
use crate::tui::help::HelpSection;
use crate::tui::nav::{ResourceId, Source};
use crate::tui::notify::Notification;
use crate::tui::screen::{ActionSpec, Goto, ScreenSpec};
use crate::tui::template::{Scope, TemplateError};

/// Stable id of a view on the stack; fetch answers are routed by it.
pub type ViewId = u64;

/// Whether a view used a key; unused keys fall through to the app's
/// defaults (Esc/q pop, `r` refresh, `?` help).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handled {
    Yes,
    No,
}

/// What the renderer passes a view each frame.
#[derive(Debug, Clone, Copy)]
pub struct RenderCtx {
    pub tick: u64,
    pub now_ms: i64,
}

pub trait View {
    /// Breadcrumb text ("GZS3", "Run 42").
    fn title(&self) -> String;
    /// Called once, right after the view is pushed: start fetches here.
    fn on_open(&mut self, _ctx: &mut Ctx) {}
    fn on_key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled;
    /// The answer to a request this view sent with [`Ctx::fetch`].
    fn on_fetched(&mut self, _id: u64, _result: Result<Value, String>, _ctx: &mut Ctx) {}
    /// About ten times a second; poll with a [`Poller`] here.
    fn on_tick(&mut self, _ctx: &mut Ctx) {}
    /// `r`, and after a row action changed something.
    fn refresh(&mut self, _ctx: &mut Ctx) {}
    fn on_scroll(&mut self, _delta: isize) {}
    fn render(&mut self, frame: &mut Frame, area: Rect, ui: RenderCtx);
    /// Keys for the `?` overlay, beyond the global ones.
    fn help(&self) -> Vec<HelpSection> {
        Vec::new()
    }
    /// Footer hints: `(key, what)`.
    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        vec![("esc", "back"), ("?", "help")]
    }
    fn is_busy(&self) -> bool {
        false
    }
    /// True while the user types into the view, so `q` and `?` are text.
    fn is_typing(&self) -> bool {
        false
    }
}

/// The row an action or link acts on, and where it was started.
#[derive(Debug, Clone)]
pub struct RowContext {
    /// The row, merged with its loaded details in a detail view.
    pub row: Value,
    /// The row's title, typed to confirm destructive actions.
    pub title: Option<String>,
    /// Singular noun for messages ("connection").
    pub noun: &'static str,
    /// Its screen, for the action menu.
    pub spec: Option<&'static ScreenSpec>,
    /// Rows drilled through to reach it, nearest first.
    pub parents: Vec<Value>,
    /// Positional ids (`{0}`, `{1}`): the drill trail, then this row's id.
    pub ids: Vec<String>,
    /// The screen to refresh after the action.
    pub resource: Option<ResourceId>,
    /// The view it came from, refreshed (or closed) after the action.
    pub view: Option<ViewId>,
}

impl RowContext {
    /// Template scope: `extra` (form values) first, then the row, then its
    /// parents; `$site` is the signed-in site.
    pub fn scope<'a>(&'a self, extra: Option<&'a Value>, site: Option<&str>) -> Scope<'a> {
        let mut scope = Scope::with_ids(&self.ids);
        for parent in self.parents.iter().rev() {
            scope = scope.nearest(parent);
        }
        scope = scope.nearest(&self.row);
        if let Some(extra) = extra {
            scope = scope.nearest(extra);
        }
        match site {
            Some(site) => scope.var("site", site),
            None => scope,
        }
    }
}

/// What a view asks the app to do; applied after the view returns.
#[allow(
    dead_code,
    reason = "spec vocabulary for the screens being converted next"
)]
pub enum Effect {
    Push(Box<dyn View>),
    Pop,
    Toast(Tone, String),
    Notify(Notification),
    Copy {
        text: String,
        what: String,
    },
    RunAction(&'static ActionSpec, RowContext),
    /// Lists the row's actions and links (from `RowContext::spec`).
    ActionMenu(RowContext),
    Follow(Goto, RowContext),
    Open(ResourceId),
    /// Opens a place (Back returns) and lists it in Recent under the label.
    Go(Place, String),
}

/// A view's handle on the app for one call.
pub struct Ctx<'a> {
    pub now: Now,
    /// The signed-in site, for `{$site}` in templates.
    pub site: Option<String>,
    view: Option<ViewId>,
    next_id: &'a mut u64,
    outbox: &'a mut Vec<Request>,
    effects: Vec<Effect>,
}

impl<'a> Ctx<'a> {
    pub fn new(
        now: Now,
        site: Option<String>,
        view: Option<ViewId>,
        next_id: &'a mut u64,
        outbox: &'a mut Vec<Request>,
    ) -> Self {
        Self {
            now,
            site,
            view,
            next_id,
            outbox,
            effects: Vec::new(),
        }
    }

    /// The view this context belongs to, if any.
    pub fn view_id(&self) -> Option<ViewId> {
        self.view
    }

    /// Sends a request; the answer comes back to this view's `on_fetched`
    /// with the returned id (or, outside a view, failures are toasted).
    pub fn fetch_raw(
        &mut self,
        service: &str,
        method: Method,
        path: String,
        body: Option<Value>,
    ) -> u64 {
        let id = *self.next_id;
        *self.next_id += 1;
        let target = match self.view {
            Some(view) => FetchTarget::View(view),
            None => FetchTarget::Background,
        };
        self.outbox.push(Request::Fetch(FetchReq {
            id,
            target,
            service: service.to_string(),
            method,
            path,
            body,
        }));
        id
    }

    /// Fills `source` from `scope` and sends it.
    pub fn fetch(&mut self, source: &Source, scope: &Scope) -> Result<u64, TemplateError> {
        let (path, body) = source.resolve(scope)?;
        Ok(self.fetch_raw(source.service, source.verb.method(), path, body))
    }

    #[allow(
        dead_code,
        reason = "spec vocabulary for the screens being converted next"
    )]
    pub fn push(&mut self, view: Box<dyn View>) {
        self.effects.push(Effect::Push(view));
    }

    #[allow(
        dead_code,
        reason = "spec vocabulary for the screens being converted next"
    )]
    pub fn pop(&mut self) {
        self.effects.push(Effect::Pop);
    }

    pub fn toast(&mut self, tone: Tone, text: impl Into<String>) {
        self.effects.push(Effect::Toast(tone, text.into()));
    }

    #[allow(
        dead_code,
        reason = "spec vocabulary for the screens being converted next"
    )]
    pub fn notify(&mut self, notification: Notification) {
        self.effects.push(Effect::Notify(notification));
    }

    /// Copies `text` to the clipboard (OSC 52); `what` names it in the toast.
    pub fn copy(&mut self, text: impl Into<String>, what: impl Into<String>) {
        self.effects.push(Effect::Copy {
            text: text.into(),
            what: what.into(),
        });
    }

    pub fn effect(&mut self, effect: Effect) {
        self.effects.push(effect);
    }

    pub fn into_effects(self) -> Vec<Effect> {
        self.effects
    }
}

/// Views on top of the browser, last = on screen.
#[derive(Default)]
pub struct ViewStack {
    views: Vec<(ViewId, Box<dyn View>)>,
    next_id: ViewId,
}

impl ViewStack {
    pub fn push(&mut self, view: Box<dyn View>) -> ViewId {
        self.next_id += 1;
        self.views.push((self.next_id, view));
        self.next_id
    }

    pub fn pop(&mut self) -> Option<Box<dyn View>> {
        self.views.pop().map(|(_, view)| view)
    }

    pub fn clear(&mut self) {
        self.views.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.views.is_empty()
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.views.len()
    }

    pub fn top(&self) -> Option<(ViewId, &dyn View)> {
        self.views.last().map(|(id, view)| (*id, view.as_ref()))
    }

    pub fn top_mut(&mut self) -> Option<(ViewId, &mut Box<dyn View>)> {
        self.views.last_mut().map(|(id, view)| (*id, view))
    }

    pub fn get_mut(&mut self, id: ViewId) -> Option<&mut Box<dyn View>> {
        self.views
            .iter_mut()
            .find(|(view_id, _)| *view_id == id)
            .map(|(_, view)| view)
    }

    pub fn ids(&self) -> Vec<ViewId> {
        self.views.iter().map(|(id, _)| *id).collect()
    }

    pub fn titles(&self) -> Vec<String> {
        self.views.iter().map(|(_, view)| view.title()).collect()
    }

    pub fn any_busy(&self) -> bool {
        self.views.iter().any(|(_, view)| view.is_busy())
    }
}
