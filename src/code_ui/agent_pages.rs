//! One page per subagent: everything a subagent does
//! (its task, tool calls, progress text and result) goes on its own
//! full-screen page, so the main page shows only the main agent's work and
//! one line per subagent. A tab bar names `main` and every subagent, with
//! the page on screen highlighted.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::assistant::tools::ToolName;
use crate::assistant::AssistantEvent;
use crate::code_ui::items::{Item, ToolItem, ToolStatus};
use crate::code_ui::labels;
use crate::code_ui::state::SCRIPT_CHILD_PREFIX;
use crate::code_ui::transcript::Viewer;
use crate::subagents::render as subagent_render;
use crate::subagents::{family_order, Board, Status, Subagent, SubagentEvent, ToolState};
use crate::tui::theme;

/// The subagent page on screen and its scroll position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentView {
    pub index: usize,
    pub viewer: Viewer,
}

impl AgentView {
    pub fn open(index: usize) -> Self {
        Self {
            index,
            viewer: Viewer::open(),
        }
    }
}

/// Before the name on the tab of a subagent another subagent started.
const CHILD_MARK: &str = "↳";

/// The keys that switch pages, shown wherever the tab bar is.
pub const SWITCH_HINT: &str = "shift+↓ view agents";

/// Everything one subagent did, in order.
#[derive(Debug, Clone)]
pub struct AgentPage {
    /// The latest state of the subagent (status, stats, task).
    pub agent: Subagent,
    items: Vec<Item>,
    /// Local tools in flight (run by gz for this subagent).
    running: Vec<ToolItem>,
    /// Server tools in flight, by tool name.
    server: Vec<(String, ToolItem)>,
    /// Progress text since the last tool call.
    text: String,
}

impl AgentPage {
    fn new(agent: Subagent) -> Self {
        Self {
            agent,
            items: Vec::new(),
            running: Vec::new(),
            server: Vec::new(),
            text: String::new(),
        }
    }

    fn close_text(&mut self) {
        let text = std::mem::take(&mut self.text);
        if !text.trim().is_empty() {
            self.items.push(Item::Assistant(text.trim().to_string()));
        }
    }

    /// The page's items: the task, what happened, then what's in flight.
    pub fn items(&self) -> Vec<Item> {
        let mut items = Vec::new();
        if let Some(task) = &self.agent.task {
            items.push(Item::User(task.clone()));
        }
        items.extend(self.items.iter().cloned());
        items.extend(
            self.running
                .iter()
                .chain(self.server.iter().map(|(_, item)| item))
                .cloned()
                .map(Item::Tool),
        );
        if !self.text.trim().is_empty() {
            items.push(Item::Assistant(self.text.trim().to_string()));
        }
        if items.is_empty() {
            items.push(Item::Notice("Starting…".into()));
        }
        items
    }

    fn server_tool(&mut self, tool: &str, state: ToolState) {
        // gz runs client tools itself and reports them with full detail.
        if ToolName::parse(tool).is_some() {
            return;
        }
        let label = labels::label(tool, tool);
        if state == ToolState::Started {
            self.close_text();
            self.server.push((tool.to_string(), tool_item(label)));
            return;
        }
        let mut item = match self.server.iter().position(|(name, _)| name == tool) {
            Some(i) => self.server.remove(i).1,
            None => tool_item(label),
        };
        item.status = if state == ToolState::Failed {
            ToolStatus::Failed
        } else {
            ToolStatus::Ok
        };
        self.items.push(Item::Tool(item));
    }

    fn local_event(&mut self, event: AssistantEvent) {
        match event {
            AssistantEvent::ToolStarted { name, summary, .. } => {
                if let Some(child) = summary.strip_prefix(SCRIPT_CHILD_PREFIX) {
                    if let Some(script) = self
                        .running
                        .iter_mut()
                        .rev()
                        .find(|t| t.label.verb == "Script")
                    {
                        script.children.push(child.to_string());
                        return;
                    }
                }
                self.close_text();
                self.running.push(tool_item(labels::label(&name, &summary)));
            }
            AssistantEvent::ToolFinished {
                name,
                summary,
                is_error,
                detail,
                output,
                diff,
            } => {
                let label = labels::label(&name, &summary);
                let mut item = match self.running.iter().position(|t| t.label == label) {
                    Some(i) => self.running.remove(i),
                    None => tool_item(label),
                };
                item.status = if is_error {
                    ToolStatus::Failed
                } else {
                    ToolStatus::Ok
                };
                item.result = Some(labels::result_line(
                    &name,
                    &summary,
                    &detail,
                    diff.as_deref(),
                ));
                item.diff = diff;
                item.output = Some(output).filter(|o| !o.trim().is_empty());
                self.items.push(Item::Tool(item));
            }
            AssistantEvent::Notice(text) => {
                self.close_text();
                self.items.push(Item::Notice(text));
            }
            _ => {}
        }
    }

    fn finish(&mut self, agent: &Subagent) {
        self.close_text();
        for (_, mut item) in self.server.drain(..) {
            item.status = ToolStatus::Failed;
            self.items.push(Item::Tool(item));
        }
        let word = match agent.status {
            Status::Completed => "Done".to_string(),
            other => other.word().to_string(),
        };
        self.items.push(Item::Notice(format!(
            "{word} · {}",
            subagent_render::stats(agent, std::time::Instant::now())
        )));
        if !agent.summary.trim().is_empty() {
            self.items
                .push(Item::Assistant(agent.summary.trim().to_string()));
        }
    }
}

fn tool_item(label: labels::Label) -> ToolItem {
    ToolItem {
        label,
        status: ToolStatus::Running,
        result: None,
        diff: None,
        children: Vec::new(),
        output: None,
    }
}

/// The pages of this conversation's subagents, in start order (an open
/// page's index never moves; [`AgentPages::order`] is the order shown).
#[derive(Debug, Clone, Default)]
pub struct AgentPages {
    pages: Vec<AgentPage>,
}

impl AgentPages {
    pub fn is_empty(&self) -> bool {
        self.pages.is_empty()
    }

    pub fn get(&self, index: usize) -> Option<&AgentPage> {
        self.pages.get(index)
    }

    /// The pages' indexes in tab order: start order, with each child right
    /// after the subagent that started it.
    pub fn order(&self) -> Vec<usize> {
        let agents: Vec<&Subagent> = self.pages.iter().map(|p| &p.agent).collect();
        family_order(&agents)
    }

    /// Pages belong to one conversation; a new one starts without any.
    pub fn clear(&mut self) {
        self.pages.clear();
    }

    fn page(&mut self, id: &str, board: &Board) -> &mut AgentPage {
        let index = match self.pages.iter().position(|p| p.agent.id == id) {
            Some(i) => i,
            None => {
                let agent = board.get(id).cloned().unwrap_or_else(|| {
                    let mut empty = Board::default();
                    empty.apply(
                        &SubagentEvent::Text {
                            id: id.to_string(),
                            text: String::new(),
                        },
                        std::time::Instant::now(),
                    );
                    empty.agents.remove(0)
                });
                self.pages.push(AgentPage::new(agent));
                self.pages.len() - 1
            }
        };
        &mut self.pages[index]
    }

    /// Records a subagent event; `board` has already applied it.
    pub fn on_subagent(&mut self, event: &SubagentEvent, board: &Board) {
        let id = match event {
            SubagentEvent::Started { id, .. }
            | SubagentEvent::Tool { id, .. }
            | SubagentEvent::Text { id, .. }
            | SubagentEvent::Interrupted { id }
            | SubagentEvent::Finished { id, .. } => id.clone(),
            // Workflow nodes and handoffs aren't chat subagents.
            _ => return,
        };
        let snapshot = board.get(&id).cloned();
        let page = self.page(&id, board);
        let was_waiting = page.agent.status == Status::Waiting;
        match event {
            SubagentEvent::Started { .. } if was_waiting => {
                page.items
                    .push(Item::Notice("Approved · continuing".into()));
            }
            SubagentEvent::Tool { tool, state, .. } => page.server_tool(tool, *state),
            SubagentEvent::Text { text, .. } => page.text.push_str(text),
            SubagentEvent::Interrupted { .. } => {
                page.close_text();
                page.items.push(Item::Notice(
                    "Waiting for your approval (answer it on the main page)".into(),
                ));
            }
            _ => {}
        }
        if let Some(agent) = snapshot {
            if matches!(event, SubagentEvent::Finished { .. }) {
                page.finish(&agent);
            }
            page.agent = agent;
        }
    }

    /// Records a local tool event gz ran for subagent `id`.
    pub fn on_local(&mut self, id: &str, event: AssistantEvent, board: &Board) {
        self.page(id, board).local_event(event);
    }

    /// Updates a subagent that stopped with the turn (cancelled or failed).
    pub fn stopped(&mut self, agent: &Subagent) {
        if let Some(page) = self.pages.iter_mut().find(|p| p.agent.id == agent.id) {
            page.finish(agent);
            page.agent = agent.clone();
        }
    }
}

fn mark(agent: &Subagent, tick: u64) -> (String, Style) {
    match agent.status {
        Status::Running => (
            theme::SPINNER[tick as usize % theme::SPINNER.len()].to_string(),
            theme::brand(),
        ),
        Status::Waiting => ("◐".into(), Style::default().fg(theme::WARN)),
        Status::Completed => ("✔".into(), Style::default().fg(theme::OK)),
        Status::Failed => ("✗".into(), Style::default().fg(theme::ERROR)),
        Status::Cancelled | Status::Skipped => ("⊘".into(), Style::default().fg(theme::WARN)),
    }
}

/// `▸ main │ ⠋ Schema │ ⠋ ↳ Columns │ ✔ Lineage`: every page, the one on
/// screen (`selected`, `None` for main) highlighted, a subagent's children
/// right after it with one `↳` per level. Fits `width` by showing the
/// tabs around the selection with `‹ N` / `N ›` for the rest.
pub fn tab_bar(
    pages: &AgentPages,
    selected: Option<usize>,
    tick: u64,
    width: usize,
) -> Line<'static> {
    let chosen = |on: bool| {
        if on {
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED)
        } else {
            theme::muted()
        }
    };
    // Each tab: (mark, mark style, name, selected).
    let mut tabs: Vec<(String, Style, String, bool)> = vec![(
        "".into(),
        Style::default(),
        " main ".into(),
        selected.is_none(),
    )];
    let order = pages.order();
    for &i in &order {
        let page = &pages.pages[i];
        let (m, style) = mark(&page.agent, tick + i as u64 * 3);
        let name = match page.agent.depth {
            0 => format!(" {} ", page.agent.name),
            depth => format!(" {} {} ", CHILD_MARK.repeat(depth), page.agent.name),
        };
        tabs.push((m, style, name, selected == Some(i)));
    }
    let widths: Vec<usize> = tabs
        .iter()
        .map(|(m, _, name, _)| m.width() + name.width() + 3)
        .collect();
    let focus = selected
        .and_then(|i| order.iter().position(|&page| page == i))
        .map_or(0, |tab| tab + 1);
    let room = width.saturating_sub(8);
    // Start at the first tab; if the selection doesn't fit, end on it.
    let mut start = 0;
    while start < focus && widths[start..=focus].iter().sum::<usize>() > room {
        start += 1;
    }
    let mut used = 0;
    let mut spans = vec![Span::raw(" ")];
    if start > 0 {
        spans.push(Span::styled(format!("‹ {start} "), theme::muted()));
    }
    let mut end = start;
    for (i, (m, style, name, on)) in tabs.iter().enumerate().skip(start) {
        if used + widths[i] > room && i > focus {
            break;
        }
        if i > start {
            spans.push(Span::styled(" │ ", theme::muted()));
        }
        if !m.is_empty() {
            spans.push(Span::styled(m.clone(), *style));
        }
        spans.push(Span::styled(name.clone(), chosen(*on)));
        used += widths[i];
        end = i + 1;
    }
    if end < tabs.len() {
        spans.push(Span::styled(
            format!(" {} ›", tabs.len() - end),
            theme::muted(),
        ));
    }
    Line::from(spans)
}

/// The line under the tab bar on a subagent's page:
/// `● Task(Schema · analyst)  running · 12s · 3 tools`.
pub fn page_title(page: &AgentPage, tick: u64) -> Line<'static> {
    let (m, style) = mark(&page.agent, tick);
    Line::from(vec![
        Span::styled(format!(" {m} "), style),
        Span::styled(
            subagent_render::title(&page.agent),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                "  {} · {}",
                page.agent.status.word(),
                subagent_render::stats(&page.agent, std::time::Instant::now())
            ),
            theme::muted(),
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn started(id: &str, name: &str) -> SubagentEvent {
        SubagentEvent::Started {
            id: id.into(),
            name: name.into(),
            profile: Some("analyst".into()),
            agent_id: None,
            task: Some(format!("Look into {name}")),
            runs_on: Default::default(),
            parent: None,
        }
    }

    fn apply(pages: &mut AgentPages, board: &mut Board, event: SubagentEvent) {
        board.apply(&event, Instant::now());
        pages.on_subagent(&event, board);
    }

    fn texts(items: &[Item]) -> Vec<String> {
        items
            .iter()
            .map(|item| {
                item.render_expanded(80)
                    .iter()
                    .map(|l| {
                        l.spans
                            .iter()
                            .map(|s| s.content.as_ref())
                            .collect::<String>()
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .collect()
    }

    #[test]
    fn a_page_holds_the_task_tools_text_and_result() {
        let (mut pages, mut board) = (AgentPages::default(), Board::default());
        apply(&mut pages, &mut board, started("sa_1", "Schema"));
        apply(
            &mut pages,
            &mut board,
            SubagentEvent::Tool {
                id: "sa_1".into(),
                tool: "list_tables".into(),
                state: ToolState::Started,
                tool_use_id: None,
            },
        );
        apply(
            &mut pages,
            &mut board,
            SubagentEvent::Tool {
                id: "sa_1".into(),
                tool: "list_tables".into(),
                state: ToolState::Finished,
                tool_use_id: None,
            },
        );
        pages.on_local(
            "sa_1",
            AssistantEvent::ToolStarted {
                name: "read_file".into(),
                summary: "Read src/main.rs".into(),
                local: true,
            },
            &board,
        );
        apply(
            &mut pages,
            &mut board,
            SubagentEvent::Text {
                id: "sa_1".into(),
                text: "Found 3 tables.".into(),
            },
        );
        apply(
            &mut pages,
            &mut board,
            SubagentEvent::Finished {
                id: "sa_1".into(),
                status: Status::Completed,
                summary: "Orders has 12 columns.".into(),
                usage: Default::default(),
                duration: None,
            },
        );
        let page = pages.get(0).unwrap();
        assert_eq!(page.agent.status, Status::Completed);
        let all = texts(&page.items()).join("\n");
        for wanted in [
            "Look into Schema",
            "Found 3 tables.",
            "Done",
            "Orders has 12 columns.",
            "main.rs",
        ] {
            assert!(all.contains(wanted), "missing {wanted}: {all}");
        }
        assert!(all.to_lowercase().contains("list_tables") || all.contains("List tables"));
    }

    #[test]
    fn client_tools_announced_by_the_server_are_not_doubled() {
        let (mut pages, mut board) = (AgentPages::default(), Board::default());
        apply(&mut pages, &mut board, started("sa_1", "Files"));
        apply(
            &mut pages,
            &mut board,
            SubagentEvent::Tool {
                id: "sa_1".into(),
                tool: "read_file".into(),
                state: ToolState::Started,
                tool_use_id: None,
            },
        );
        let items = pages.get(0).unwrap().items();
        assert!(!items.iter().any(|i| matches!(i, Item::Tool(_))));
    }

    #[test]
    fn the_tab_bar_marks_main_or_the_open_page() {
        let (mut pages, mut board) = (AgentPages::default(), Board::default());
        apply(&mut pages, &mut board, started("sa_1", "Schema"));
        apply(&mut pages, &mut board, started("sa_2", "Lineage"));
        let text = |selected| -> String {
            tab_bar(&pages, selected, 0, 120)
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect()
        };
        let on_main = text(None);
        assert!(
            on_main.contains(" main ") && on_main.contains("Schema") && on_main.contains("Lineage")
        );
        let reversed = |selected| -> Vec<String> {
            tab_bar(&pages, selected, 0, 120)
                .spans
                .iter()
                .filter(|s| s.style.add_modifier.contains(Modifier::REVERSED))
                .map(|s| s.content.trim().to_string())
                .collect()
        };
        assert_eq!(reversed(None), ["main"]);
        assert_eq!(reversed(Some(1)), ["Lineage"]);
    }

    #[test]
    fn a_childs_tab_follows_its_parent() {
        let (mut pages, mut board) = (AgentPages::default(), Board::default());
        apply(&mut pages, &mut board, started("sa_1", "Schema"));
        apply(&mut pages, &mut board, started("sa_2", "Lineage"));
        let mut child = started("sa_3", "Columns");
        if let SubagentEvent::Started { parent, .. } = &mut child {
            *parent = Some("sa_1".into());
        }
        apply(&mut pages, &mut board, child);
        let bar = tab_bar(&pages, Some(2), 0, 120);
        let names: Vec<&str> = bar
            .spans
            .iter()
            .map(|s| s.content.trim())
            .filter(|name| name.chars().any(char::is_alphabetic))
            .collect();
        assert_eq!(names, ["main", "Schema", "↳ Columns", "Lineage"]);
        let open: Vec<&str> = bar
            .spans
            .iter()
            .filter(|s| s.style.add_modifier.contains(Modifier::REVERSED))
            .map(|s| s.content.trim())
            .collect();
        assert_eq!(open, ["↳ Columns"], "the open page keeps its highlight");
    }

    #[test]
    fn a_narrow_tab_bar_keeps_the_selection_and_counts_the_rest() {
        let (mut pages, mut board) = (AgentPages::default(), Board::default());
        for i in 0..12 {
            apply(
                &mut pages,
                &mut board,
                started(&format!("sa_{i}"), &format!("Agent {i}")),
            );
        }
        let line: String = tab_bar(&pages, Some(10), 0, 60)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(line.contains("Agent 10"), "{line}");
        assert!(line.contains('‹'), "{line}");
        assert!(line.width() <= 60, "{line}");
    }
}
