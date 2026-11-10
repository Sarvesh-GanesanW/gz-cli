//! The home dashboard, shown when gz opens and on `H`: what needs
//! attention (failed job runs, agent versions waiting for review), what is
//! running, AI spend, and recent chats. Each tile loads on its own, so one
//! failing endpoint leaves the others readable, and each item drills to its
//! full screen.

mod render;
#[cfg(test)]
mod tests;
pub mod tiles;

use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{layout::Rect, Frame};
use reqwest::Method;
use serde_json::Value;

use super::{Ctx, Effect, Handled, Poller, RenderCtx, RowContext, View};
use crate::spend::ledger::Summary;
use crate::tui::app::Tone;
use crate::tui::chat::Conversation;
use crate::tui::errors::{self, Subject};
use crate::tui::format;
use crate::tui::help::HelpSection;
use crate::tui::nav;
use crate::tui::palette::Action;
use crate::tui::screen::{Goto, Select, Target};
use tiles::{Approval, Runs, Spend};

/// Failed and running jobs, and approvals: cheap enough to poll each minute.
const ACTIVITY_EVERY: Duration = Duration::from_secs(60);
/// `/chat/metrics` is ~20 KB and changes slowly.
const SPEND_EVERY: Duration = Duration::from_secs(5 * 60);
/// The dashboard's breadcrumb, also how the app tells it is in front.
pub const HOME_TITLE: &str = "Home";
/// The window "failed recently" covers.
const RECENT_MS: i64 = 24 * 60 * 60 * 1000;

/// A run's own row in Catalog › Job usage.
pub const RUN_GOTO: Goto = Goto {
    target: Target {
        module: "Catalog",
        resource: "Job usage",
    },
    select: Select::Row {
        field: &["executionId"],
        from: &["executionId"],
    },
};
/// Job usage narrowed to "running" rows, for the stuck ones.
const RUNNING_GOTO: Goto = Goto {
    target: RUN_GOTO.target,
    select: Select::Filter { from: &["status"] },
};
/// The run's project, drilled into its jobs.
const PROJECT_GOTO: Goto = Goto {
    target: Target {
        module: "DE/ML",
        resource: "Projects",
    },
    select: Select::Drill {
        field: &["Id"],
        from: &["projectId"],
    },
};
/// The request in Agents › Approval requests, where it can be decided.
pub const APPROVAL_GOTO: Goto = Goto {
    target: Target {
        module: "Agents",
        resource: "Approval requests",
    },
    select: Select::Row {
        field: &["id"],
        from: &["id"],
    },
};
const MY_SPEND: Target = Target {
    module: "Chat",
    resource: "Metrics",
};
const SITE_SPEND: Target = Target {
    module: "Catalog",
    resource: "AI usage",
};
const BUDGET: Target = Target {
    module: "Chat",
    resource: "Budget",
};

/// Every screen the dashboard leads to, for the test that keeps them valid.
#[cfg(test)]
pub const TARGETS: &[Target] = &[
    RUN_GOTO.target,
    PROJECT_GOTO.target,
    APPROVAL_GOTO.target,
    MY_SPEND,
    SITE_SPEND,
    BUDGET,
];

/// One endpoint's state: the last answer is kept while a refresh runs.
#[derive(Debug, Clone)]
struct Tile<T> {
    data: Option<T>,
    awaiting: Option<u64>,
    error: Option<String>,
    /// The user may not see this (401/403 from a permission check).
    denied: bool,
}

impl<T> Default for Tile<T> {
    fn default() -> Self {
        Self {
            data: None,
            awaiting: None,
            error: None,
            denied: false,
        }
    }
}

impl<T> Tile<T> {
    fn settle(&mut self, result: Result<T, String>, subject: Subject) {
        self.awaiting = None;
        match result {
            Ok(data) => {
                self.data = Some(data);
                self.error = None;
                self.denied = false;
            }
            Err(err) => {
                self.denied = errors::is_access_denied(&err, subject.service);
                self.error = Some(errors::explain(&err, subject));
            }
        }
    }

    fn is_loading(&self) -> bool {
        self.awaiting.is_some() && self.data.is_none()
    }
}

/// The dashboard's boxes, in Tab order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    Attention,
    Running,
    Spend,
    Chats,
}

const PANELS: [Panel; 4] = [Panel::Attention, Panel::Running, Panel::Spend, Panel::Chats];

/// What Enter acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pick {
    FailedRun(usize),
    Approval(usize),
    Running(usize),
    Stuck,
    MySpend,
    SiteSpend,
    Budget,
    Chat(usize),
}

pub struct HomeView {
    runs: Tile<Runs>,
    approvals: Tile<Vec<Approval>>,
    spend: Tile<Spend>,
    /// gz code's spend today and this month on this machine.
    local: Option<Summary>,
    chats: Tile<Vec<Conversation>>,
    panel: Panel,
    /// Selected item within each panel, by panel index.
    selected: [usize; PANELS.len()],
    activity: Poller,
    spend_poller: Poller,
}

impl Default for HomeView {
    fn default() -> Self {
        Self::new()
    }
}

impl HomeView {
    pub fn new() -> Self {
        Self {
            runs: Tile::default(),
            approvals: Tile::default(),
            spend: Tile::default(),
            local: None,
            chats: Tile::default(),
            panel: Panel::Attention,
            selected: [0; PANELS.len()],
            activity: Poller::every(ACTIVITY_EVERY),
            spend_poller: Poller::every(SPEND_EVERY),
        }
    }

    fn load_activity(&mut self, ctx: &mut Ctx) {
        let since = format::iso8601_utc(ctx.now.epoch_ms - RECENT_MS);
        // One call covers failed and open runs: open runs overlap any window.
        let runs = format!("/etl/consumption-metrics?workloadType=job&from={since}&limit=100");
        self.runs.awaiting = Some(ctx.fetch_raw("etl", Method::GET, runs, None));
        let approvals = format!(
            "/v1/approval-requests?status=waiting_for_approval&limit={}",
            tiles::APPROVALS_PAGE
        );
        self.approvals.awaiting = Some(ctx.fetch_raw("agents", Method::GET, approvals, None));
    }

    fn load_spend(&mut self, ctx: &mut Ctx) {
        let path = format!("/chat/metrics?from={}", month_start(ctx.now.epoch_ms));
        self.spend.awaiting = Some(ctx.fetch_raw("chat", Method::GET, path, None));
        self.local = ctx.local_spend();
    }

    fn load_chats(&mut self, ctx: &mut Ctx) {
        let path = "/chat/conversations?assistantMode=chat".to_string();
        self.chats.awaiting = Some(ctx.fetch_raw("chat", Method::GET, path, None));
    }

    fn load_all(&mut self, ctx: &mut Ctx) {
        self.load_activity(ctx);
        self.load_spend(ctx);
        self.load_chats(ctx);
        self.activity.restart(ctx.now.instant);
        self.spend_poller.restart(ctx.now.instant);
    }

    /// The selectable items of `panel`, top to bottom.
    pub fn picks(&self, panel: Panel) -> Vec<Pick> {
        match panel {
            Panel::Attention => {
                let failed = self.runs.data.as_ref().map_or(0, |r| r.failed.len());
                let approvals = self.approvals.data.as_ref().map_or(0, Vec::len);
                (0..failed.min(render::LIST_LIMIT))
                    .map(Pick::FailedRun)
                    .chain((0..approvals.min(render::LIST_LIMIT)).map(Pick::Approval))
                    .collect()
            }
            Panel::Running => {
                let Some(runs) = &self.runs.data else {
                    return Vec::new();
                };
                let mut picks: Vec<Pick> = (0..runs.running.len().min(render::LIST_LIMIT))
                    .map(Pick::Running)
                    .collect();
                if !runs.stuck.is_empty() {
                    picks.push(Pick::Stuck);
                }
                picks
            }
            Panel::Spend if self.spend.data.is_some() => {
                vec![Pick::MySpend, Pick::SiteSpend, Pick::Budget]
            }
            Panel::Spend => Vec::new(),
            Panel::Chats => {
                let chats = self.chats.data.as_ref().map_or(0, Vec::len);
                (0..chats).map(Pick::Chat).collect()
            }
        }
    }

    fn panel_index(&self) -> usize {
        PANELS.iter().position(|p| *p == self.panel).unwrap_or(0)
    }

    /// The item Enter would open, if the panel in focus has one.
    pub fn selected_pick(&self) -> Option<Pick> {
        let picks = self.picks(self.panel);
        let index = self.selected[self.panel_index()].min(picks.len().checked_sub(1)?);
        picks.get(index).copied()
    }

    fn move_selection(&mut self, delta: isize) {
        let len = self.picks(self.panel).len();
        let index = self.panel_index();
        if len == 0 {
            return;
        }
        let current = self.selected[index].min(len - 1) as isize;
        self.selected[index] = (current + delta).clamp(0, len as isize - 1) as usize;
    }

    fn cycle_panel(&mut self, forward: bool) {
        let step = if forward { 1 } else { PANELS.len() - 1 };
        self.panel = PANELS[(self.panel_index() + step) % PANELS.len()];
    }

    fn open(&self, pick: Pick, ctx: &mut Ctx) {
        let runs = self.runs.data.as_ref();
        let follow = |goto: Goto, row: &Value, ctx: &mut Ctx| {
            ctx.effect(Effect::Follow(goto, RowContext::detached(row.clone())));
        };
        match pick {
            Pick::FailedRun(i) => {
                if let Some(run) = runs.and_then(|r| r.failed.get(i)) {
                    follow(RUN_GOTO, &run.row, ctx);
                }
            }
            Pick::Running(i) => {
                if let Some(run) = runs.and_then(|r| r.running.get(i)) {
                    follow(RUN_GOTO, &run.row, ctx);
                }
            }
            Pick::Stuck => {
                if let Some(run) = runs.and_then(|r| r.stuck.first()) {
                    follow(RUNNING_GOTO, &run.row, ctx);
                }
            }
            Pick::Approval(i) => {
                if let Some(approval) = self.approvals.data.as_ref().and_then(|a| a.get(i)) {
                    follow(APPROVAL_GOTO, &approval.row, ctx);
                }
            }
            Pick::MySpend => open_target(MY_SPEND, ctx),
            Pick::SiteSpend => open_target(SITE_SPEND, ctx),
            Pick::Budget => open_target(BUDGET, ctx),
            Pick::Chat(i) => {
                if let Some(chat) = self.chats.data.as_ref().and_then(|c| c.get(i)) {
                    ctx.effect(Effect::Command(Action::OpenConversation {
                        id: chat.id.clone(),
                        title: chat.title.clone(),
                    }));
                }
            }
        }
    }

    /// `p`: the selected run's project and its jobs.
    fn open_project(&self, ctx: &mut Ctx) {
        let runs = self.runs.data.as_ref();
        let run = match self.selected_pick() {
            Some(Pick::FailedRun(i)) => runs.and_then(|r| r.failed.get(i)),
            Some(Pick::Running(i)) => runs.and_then(|r| r.running.get(i)),
            _ => None,
        };
        match run {
            Some(run) => ctx.effect(Effect::Follow(
                PROJECT_GOTO,
                RowContext::detached(run.row.clone()),
            )),
            None => ctx.toast(Tone::Info, "Select a job run first"),
        }
    }
}

fn open_target(target: Target, ctx: &mut Ctx) {
    match nav::find_resource(target.module, target.resource) {
        Some(resource) => ctx.effect(Effect::Open(resource)),
        None => ctx.toast(
            Tone::Error,
            format!("{} › {} no longer exists", target.module, target.resource),
        ),
    }
}

/// "2026-11-01T00:00:00Z" for the month `epoch_ms` falls in (UTC).
fn month_start(epoch_ms: i64) -> String {
    let now = format::iso8601_utc(epoch_ms);
    format!("{}-01T00:00:00Z", &now[..7])
}

impl View for HomeView {
    fn title(&self) -> String {
        HOME_TITLE.to_string()
    }

    fn on_open(&mut self, ctx: &mut Ctx) {
        self.load_all(ctx);
    }

    fn on_key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled {
        match key.code {
            KeyCode::Tab => self.cycle_panel(true),
            KeyCode::BackTab => self.cycle_panel(false),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => match self.selected_pick() {
                Some(pick) => self.open(pick, ctx),
                None => ctx.toast(Tone::Info, "Nothing to open here"),
            },
            KeyCode::Char('p') => self.open_project(ctx),
            KeyCode::Char('n') => ctx.effect(Effect::Command(Action::NewChat)),
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn on_fetched(&mut self, id: u64, result: Result<Value, String>, ctx: &mut Ctx) {
        let now_ms = ctx.now.epoch_ms;
        let subject = |what, service| Subject { what, service };
        if self.runs.awaiting == Some(id) {
            let parsed = result.map(|answer| tiles::runs(&answer, now_ms));
            self.runs.settle(parsed, subject("job runs", "etl"));
        } else if self.approvals.awaiting == Some(id) {
            let parsed = result.map(|answer| tiles::approvals(&answer));
            self.approvals
                .settle(parsed, subject("agent approval requests", "agents"));
        } else if self.spend.awaiting == Some(id) {
            let parsed = result.map(|answer| tiles::spend(&answer));
            self.spend.settle(parsed, subject("AI usage", "chat"));
        } else if self.chats.awaiting == Some(id) {
            let parsed = result.map(|answer| tiles::recent_chats(&answer));
            self.chats
                .settle(parsed, subject("your conversations", "chat"));
        }
    }

    fn on_tick(&mut self, ctx: &mut Ctx) {
        let now = ctx.now.instant;
        if self.activity.due(now) && self.runs.awaiting.is_none() {
            self.load_activity(ctx);
        }
        if self.spend_poller.due(now) && self.spend.awaiting.is_none() {
            self.load_spend(ctx);
        }
    }

    fn refresh(&mut self, ctx: &mut Ctx) {
        self.load_all(ctx);
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, ui: RenderCtx) {
        render::render(self, frame, area, ui);
    }

    fn help(&self) -> Vec<HelpSection> {
        vec![HelpSection::new(
            "Home",
            &[
                ("tab  shift+tab", "next / previous tile"),
                ("↑ ↓  j k", "move within the tile"),
                ("⏎  →", "open the selected item"),
                ("p", "the selected run's project"),
                ("n", "new chat"),
                ("r", "refresh every tile"),
                ("esc", "browse"),
            ],
        )]
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        vec![
            ("tab", "next tile"),
            ("⏎", "open"),
            ("n", "new chat"),
            ("r", "refresh"),
            ("esc", "browse"),
            ("?", "help"),
        ]
    }

    fn stands_alone(&self) -> bool {
        true
    }

    fn is_busy(&self) -> bool {
        [
            self.runs.awaiting,
            self.approvals.awaiting,
            self.spend.awaiting,
            self.chats.awaiting,
        ]
        .iter()
        .any(Option::is_some)
    }
}
