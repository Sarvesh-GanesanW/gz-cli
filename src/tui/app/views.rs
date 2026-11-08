//! The view stack inside the app: pushing views, routing keys, answers and
//! ticks to them, and carrying out what they ask for.

use crossterm::event::{KeyCode, KeyEvent};
use serde_json::Value;

use crate::tui::fetch::Request;
use crate::tui::notify::Jump;
use crate::tui::overlay::menu::{self, ActionMenu};
use crate::tui::palette::{Action, Entry};
use crate::tui::views::{Ctx, Effect, Handled, View, ViewId};

use super::{App, Overlay, Tone};

impl App {
    /// Puts `view` on top and lets it start loading.
    pub fn push_view(&mut self, view: Box<dyn View>) -> ViewId {
        self.editing_filter = false;
        let id = self.views.push(view);
        self.with_view(id, |view, ctx| view.on_open(ctx));
        id
    }

    /// Runs `f` on view `id` with a context, then applies its effects.
    pub(super) fn with_view<R>(
        &mut self,
        id: ViewId,
        f: impl FnOnce(&mut dyn View, &mut Ctx) -> R,
    ) -> Option<R> {
        let site = self.resolved.site();
        let view = self.views.get_mut(id)?;
        let mut ctx = Ctx::new(
            self.now,
            site,
            Some(id),
            &mut self.next_id,
            &mut self.outbox,
        );
        let result = f(view.as_mut(), &mut ctx);
        let effects = ctx.into_effects();
        self.apply(effects);
        Some(result)
    }

    /// A context for code outside any view (custom row actions).
    pub(super) fn run_detached(&mut self, f: impl FnOnce(&mut Ctx)) {
        let site = self.resolved.site();
        let mut ctx = Ctx::new(self.now, site, None, &mut self.next_id, &mut self.outbox);
        f(&mut ctx);
        let effects = ctx.into_effects();
        self.apply(effects);
    }

    pub(super) fn apply(&mut self, effects: Vec<Effect>) {
        for effect in effects {
            match effect {
                Effect::Push(view) => {
                    self.push_view(view);
                }
                Effect::Pop => {
                    self.views.pop();
                }
                Effect::Toast(tone, text) => self.set_status(tone, text),
                Effect::Notify(notification) => self.notifier.push(notification),
                Effect::Copy { text, what } => {
                    self.outbox.push(Request::Copy(text));
                    self.set_status(Tone::Ok, format!("Copied {what}"));
                }
                Effect::RunAction(action, row) => self.start_action(action, row),
                Effect::ActionMenu(row) => self.open_action_menu(row),
                Effect::Follow(goto, row) => self.follow(goto, &row),
                Effect::Open(resource) => {
                    self.views.clear();
                    self.open_and_focus(resource);
                }
                Effect::Go(place, label) => {
                    self.go_to(&place);
                    self.remember(Entry {
                        label,
                        hint: "recent",
                        action: Action::Go(place),
                    });
                }
            }
        }
    }

    /// The `a` menu for a row; says so when the row offers nothing.
    pub(super) fn open_action_menu(&mut self, row: super::RowContext) {
        let items = row
            .spec
            .map(|spec| menu::items_for(spec, &row.row))
            .unwrap_or_default();
        if items.is_empty() {
            self.set_status(Tone::Info, "No actions for this row");
            return;
        }
        let title = row.title.clone().unwrap_or_else(|| row.noun.to_string());
        self.overlay = Some(Overlay::Menu(ActionMenu::new(title, items, row)));
    }

    /// Keys for the view on top. Unhandled Esc/q pop it, `r` refreshes it
    /// and `?` opens help.
    pub(super) fn on_view_key(&mut self, key: KeyEvent) {
        let Some((id, view)) = self.views.top() else {
            return;
        };
        let typing = view.is_typing();
        if self.with_view(id, |view, ctx| view.on_key(key, ctx)) != Some(Handled::No) {
            return;
        }
        match key.code {
            KeyCode::Esc => {
                self.views.pop();
            }
            KeyCode::Char('q') | KeyCode::Backspace | KeyCode::Left if !typing => {
                self.views.pop();
            }
            KeyCode::Char('?') if !typing => self.help = true,
            KeyCode::Char('/') if !typing => self.open_search(""),
            KeyCode::Char('r') if !typing => {
                self.with_view(id, |view, ctx| view.refresh(ctx));
            }
            _ => {}
        }
    }

    pub(super) fn view_fetched(&mut self, view: ViewId, id: u64, result: Result<Value, String>) {
        // Answers for a popped view have nowhere to go and are dropped.
        self.with_view(view, |v, ctx| v.on_fetched(id, result, ctx));
    }

    pub(super) fn tick_views(&mut self) {
        for id in self.views.ids() {
            self.with_view(id, |view, ctx| view.on_tick(ctx));
        }
    }

    /// Reloads the screen in front when its spec polls and the interval is up.
    pub(super) fn poll_grid(&mut self) {
        if !self.views.is_empty() {
            return;
        }
        let Some(every) = self.screen().and_then(|s| s.poll) else {
            return;
        };
        let now = self.now.instant;
        let Some(view) = self.view_mut() else { return };
        let due = view
            .poller
            .get_or_insert_with(|| crate::tui::views::Poller::every(every))
            .due(now);
        if due && !view.is_loading() {
            self.reload();
        }
    }

    /// Opens what the notice in the status bar points at.
    pub(super) fn jump_to_notice(&mut self) {
        match self.notifier.take_jump() {
            Some(Jump::Resource(resource)) => {
                self.views.clear();
                self.open_and_focus(resource);
            }
            Some(Jump::View(build)) => {
                self.push_view(build());
            }
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crossterm::event::KeyCode;
    use ratatui::{layout::Rect, Frame};
    use serde_json::json;

    use super::*;
    use crate::tui::app::test_support::*;
    use crate::tui::fetch::FetchTarget;
    use crate::tui::notify::Notification;
    use crate::tui::theme::BadgeTone;
    use crate::tui::views::{Poller, RenderCtx};

    /// A live view: fetches on open and every two seconds after.
    struct Live {
        poller: Poller,
        answers: usize,
    }

    impl View for Live {
        fn title(&self) -> String {
            "Live".into()
        }
        fn on_open(&mut self, ctx: &mut Ctx) {
            ctx.fetch_raw("etl", reqwest::Method::GET, "/live".into(), None);
        }
        fn on_key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled {
            match key.code {
                KeyCode::Char('n') => {
                    ctx.notify(Notification::new(BadgeTone::Good, "Run finished"));
                    Handled::Yes
                }
                _ => Handled::No,
            }
        }
        fn on_fetched(&mut self, _id: u64, _result: Result<Value, String>, _ctx: &mut Ctx) {
            self.answers += 1;
        }
        fn on_tick(&mut self, ctx: &mut Ctx) {
            if self.poller.due(ctx.now.instant) {
                ctx.fetch_raw("etl", reqwest::Method::GET, "/live".into(), None);
            }
        }
        fn render(&mut self, _frame: &mut Frame, _area: Rect, _ui: RenderCtx) {}
    }

    fn live() -> Box<dyn View> {
        Box::new(Live {
            poller: Poller::every(Duration::from_secs(2)),
            answers: 0,
        })
    }

    fn live_fetches(app: &App) -> usize {
        app.outbox
            .iter()
            .filter(|r| matches!(r, Request::Fetch(f) if f.path == "/live"))
            .count()
    }

    #[test]
    fn views_push_fetch_and_pop_on_escape() {
        let mut app = app();
        let id = app.push_view(live());
        assert_eq!(app.views.len(), 1);
        let req = last_fetch(&app);
        assert_eq!(req.target, FetchTarget::View(id));
        assert_eq!(app.breadcrumb().last().unwrap(), "Live");
        press(&mut app, &[KeyCode::Esc]);
        assert!(app.views.is_empty());
        // The answer for the popped view is dropped quietly.
        app.fetched(req.id, req.target, Ok(json!({})));
    }

    #[test]
    fn polling_runs_while_the_view_is_open_and_stops_when_popped() {
        let mut app = app();
        let start = app.now;
        app.push_view(live());
        assert_eq!(live_fetches(&app), 1);
        app.on_tick(start);
        app.on_tick(start.advanced(Duration::from_secs(1)));
        assert_eq!(live_fetches(&app), 1);
        app.on_tick(start.advanced(Duration::from_secs(2)));
        assert_eq!(live_fetches(&app), 2);
        press(&mut app, &[KeyCode::Char('q')]);
        app.on_tick(start.advanced(Duration::from_secs(10)));
        assert_eq!(live_fetches(&app), 2, "popped views stop polling");
    }

    #[test]
    fn views_raise_notifications_that_jump_back() {
        let mut app = app();
        app.push_view(live());
        press(&mut app, &[KeyCode::Char('n')]);
        app.on_tick(app.now);
        assert_eq!(app.notifier.showing().unwrap().text, "Run finished");
        let target = resource("Designer", "Datasets");
        app.notifier
            .push(Notification::new(BadgeTone::Info, "x").jump_to(Jump::Resource(target)));
        app.on_tick(app.now.advanced(crate::tui::notify::SHOW_FOR));
        app.on_key(crossterm::event::KeyEvent::new(
            KeyCode::Char('g'),
            crossterm::event::KeyModifiers::CONTROL,
        ));
        assert!(app.views.is_empty());
        assert_eq!(app.active, target);
    }
}
