//! The model's questions and plans, like Claude Code's AskUserQuestion and
//! plan approval: one question at a time with its options and "type your
//! own"; for a plan, approve it (choosing how gz works next) or keep
//! planning with feedback. Prompts for permissions take precedence.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::oneshot;

use crate::assistant::questions::{Query, Question, Reply, WorkMode};
use crate::code_ui::approver::{Mode, UiQuery};
use crate::code_ui::items::Item;

use super::{Effect, UiState};

/// One row the open question or plan offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryOption {
    Pick {
        label: String,
        description: Option<String>,
    },
    /// "Type something else": opens the text field.
    Other,
    Approve(WorkMode),
    /// "No, keep planning": opens the feedback field.
    KeepPlanning,
}

impl QueryOption {
    pub fn label(&self) -> &str {
        match self {
            QueryOption::Pick { label, .. } => label,
            QueryOption::Other => "Type something else…",
            QueryOption::Approve(WorkMode::Auto) => "Yes, and use auto mode",
            QueryOption::Approve(WorkMode::AcceptEdits) => "Yes, and accept edits",
            QueryOption::Approve(WorkMode::Ask) => "Yes, and ask before edits and commands",
            QueryOption::KeepPlanning => "No, keep planning (tell gz what to change)",
        }
    }
}

/// The question or plan on screen.
pub struct QueryView {
    pub query: Query,
    reply: Option<oneshot::Sender<Reply>>,
    /// The question shown, for a list of questions.
    pub step: usize,
    pub selected: usize,
    /// Ticked options, for a multi-select question.
    pub picked: Vec<bool>,
    answers: Vec<Option<String>>,
    /// `Some` while typing an answer or plan feedback.
    pub typing: Option<String>,
    offer_auto: bool,
}

impl QueryView {
    fn question(&self) -> Option<&Question> {
        match &self.query {
            Query::Questions(questions) => questions.get(self.step),
            Query::Plan(_) => None,
        }
    }

    /// The bold line on top: the question's tag, or the plan's title.
    pub fn title(&self) -> String {
        match (&self.query, self.question()) {
            (Query::Questions(questions), Some(q)) if questions.len() > 1 => {
                format!("{} · {}/{}", q.header, self.step + 1, questions.len())
            }
            (_, Some(q)) => q.header.clone(),
            (Query::Plan(_), None) | (Query::Questions(_), None) => "Ready to start?".to_string(),
        }
    }

    pub fn text(&self) -> String {
        match self.question() {
            Some(q) => q.question.clone(),
            None => "gz has finished planning (the plan is above). Go ahead?".to_string(),
        }
    }

    pub fn is_multi_select(&self) -> bool {
        self.question().is_some_and(|q| q.multi_select)
    }

    pub fn options(&self) -> Vec<QueryOption> {
        if let Some(question) = self.question() {
            let mut rows: Vec<QueryOption> = question
                .options
                .iter()
                .map(|choice| QueryOption::Pick {
                    label: choice.label.clone(),
                    description: choice.description.clone(),
                })
                .collect();
            rows.push(QueryOption::Other);
            return rows;
        }
        let mut rows = Vec::new();
        if self.offer_auto {
            rows.push(QueryOption::Approve(WorkMode::Auto));
        }
        rows.extend([
            QueryOption::Approve(WorkMode::AcceptEdits),
            QueryOption::Approve(WorkMode::Ask),
            QueryOption::KeepPlanning,
        ]);
        rows
    }
}

impl UiState {
    /// Shows `query`, or queues it behind the one already open.
    pub fn open_query(&mut self, query: UiQuery) {
        if self.query.is_some() {
            self.pending_queries.push_back(query);
            return;
        }
        self.agent_view = None;
        self.close_text();
        self.close_group();
        if let Query::Plan(plan) = &query.query {
            // The whole plan goes into the transcript; the box only asks.
            self.record(Item::Notice("Plan".into()));
            self.record(Item::Assistant(plan.clone()));
        }
        let picked = match &query.query {
            Query::Questions(questions) => {
                vec![false; questions.first().map_or(0, |q| q.options.len())]
            }
            Query::Plan(_) => Vec::new(),
        };
        self.query = Some(QueryView {
            query: query.query,
            reply: Some(query.reply),
            step: 0,
            selected: 0,
            picked,
            answers: Vec::new(),
            typing: None,
            offer_auto: self.auto_available(),
        });
    }

    pub(super) fn on_query_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            self.close_query(Reply::NoAnswer);
            return vec![Effect::Cancel];
        }
        let Some(view) = self.query.as_mut() else {
            return Vec::new();
        };
        if let Some(text) = view.typing.as_mut() {
            match key.code {
                KeyCode::Enter => {
                    let text = std::mem::take(text).trim().to_string();
                    view.typing = None;
                    self.typed_answer(text);
                }
                KeyCode::Esc => view.typing = None,
                KeyCode::Backspace => {
                    text.pop();
                }
                KeyCode::Char(c) if !ctrl => text.push(c),
                _ => {}
            }
            return Vec::new();
        }
        let last = view.options().len().saturating_sub(1);
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => view.selected = view.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => view.selected = (view.selected + 1).min(last),
            KeyCode::Char(' ') if view.is_multi_select() => {
                let selected = view.selected;
                if let Some(tick) = view.picked.get_mut(selected) {
                    *tick = !*tick;
                }
            }
            KeyCode::Char(c @ '1'..='9') => {
                let index = c as usize - '1' as usize;
                if index <= last {
                    view.selected = index;
                    self.choose_query_option(index);
                }
            }
            KeyCode::Enter => {
                let selected = view.selected;
                self.choose_query_option(selected);
            }
            KeyCode::Esc => self.skip_query_step(),
            _ => {}
        }
        Vec::new()
    }

    fn choose_query_option(&mut self, index: usize) {
        let Some(view) = self.query.as_mut() else {
            return;
        };
        let Some(option) = view.options().get(index).cloned() else {
            return;
        };
        match option {
            QueryOption::Other | QueryOption::KeepPlanning => view.typing = Some(String::new()),
            QueryOption::Approve(mode) => self.approve_plan(mode),
            QueryOption::Pick { label, .. } if !view.is_multi_select() => {
                self.answer_step(Some(label))
            }
            QueryOption::Pick { .. } => {
                let labels: Vec<String> = view
                    .options()
                    .iter()
                    .zip(&view.picked)
                    .filter(|(_, ticked)| **ticked)
                    .map(|(option, _)| option.label().to_string())
                    .collect();
                // Enter without ticks takes the highlighted option.
                let answer = if labels.is_empty() {
                    option.label().to_string()
                } else {
                    labels.join(", ")
                };
                self.answer_step(Some(answer));
            }
        }
    }

    /// The text typed for "something else", or as plan feedback.
    fn typed_answer(&mut self, text: String) {
        let is_plan = self
            .query
            .as_ref()
            .is_some_and(|v| matches!(v.query, Query::Plan(_)));
        if is_plan {
            self.record(Item::Notice(format!(
                "⎿  Keep planning{}",
                feedback_suffix(&text)
            )));
            self.close_query(Reply::KeepPlanning(Some(text).filter(|t| !t.is_empty())));
        } else {
            self.answer_step(Some(text).filter(|t| !t.is_empty()));
        }
    }

    /// Esc: skips the question, or keeps planning.
    fn skip_query_step(&mut self) {
        let is_plan = self
            .query
            .as_ref()
            .is_some_and(|v| matches!(v.query, Query::Plan(_)));
        if is_plan {
            self.record(Item::Notice("⎿  Keep planning".into()));
            self.close_query(Reply::KeepPlanning(None));
        } else {
            self.answer_step(None);
        }
    }

    /// Records this question's answer and moves to the next, or replies.
    fn answer_step(&mut self, answer: Option<String>) {
        let Some(view) = self.query.as_mut() else {
            return;
        };
        let Query::Questions(questions) = &view.query else {
            return;
        };
        let asked = questions[view.step].question.clone();
        let shown = answer.clone().unwrap_or_else(|| "(skipped)".to_string());
        view.answers.push(answer);
        view.step += 1;
        view.selected = 0;
        let next_options = questions.get(view.step).map(|q| q.options.len());
        self.record(Item::Notice(format!("⎿  {asked} → {shown}")));
        match next_options {
            Some(count) => {
                if let Some(view) = self.query.as_mut() {
                    view.picked = vec![false; count];
                }
            }
            None => {
                let answers = self.query.as_mut().map(|v| std::mem::take(&mut v.answers));
                self.close_query(Reply::Answers(answers.unwrap_or_default()));
            }
        }
    }

    fn approve_plan(&mut self, mode: WorkMode) {
        match mode {
            WorkMode::Auto => self.set_auto(true),
            WorkMode::AcceptEdits | WorkMode::Ask => {
                if let Ok(mut current) = self.mode.lock() {
                    *current = if mode == WorkMode::AcceptEdits {
                        Mode::AcceptEdits
                    } else {
                        Mode::Ask
                    };
                }
                self.record(Item::Notice("⎿  Plan approved; plan mode off".into()));
            }
        }
        self.close_query(Reply::PlanApproved(mode));
    }

    /// Sends `reply` and shows the next waiting question, if any.
    fn close_query(&mut self, reply: Reply) {
        if let Some(mut view) = self.query.take() {
            if let Some(sender) = view.reply.take() {
                sender.send(reply).ok();
            }
        }
        if let Some(next) = self.pending_queries.pop_front() {
            self.open_query(next);
        }
    }

    /// The turn ended: its questions can't be answered any more.
    pub(super) fn drop_queries(&mut self) {
        self.query = None;
        self.pending_queries.clear();
    }
}

fn feedback_suffix(text: &str) -> String {
    if text.is_empty() {
        String::new()
    } else {
        format!(": {text}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assistant::questions::Choice;
    use crate::code_ui::state::test_support::*;

    fn questions() -> Query {
        let choice = |label: &str| Choice {
            label: label.into(),
            description: None,
        };
        Query::Questions(vec![
            Question {
                question: "Which database?".into(),
                header: "Database".into(),
                options: vec![choice("Postgres"), choice("SQLite")],
                multi_select: false,
            },
            Question {
                question: "Which checks?".into(),
                header: "Checks".into(),
                options: vec![choice("Lint"), choice("Tests")],
                multi_select: true,
            },
        ])
    }

    fn open(s: &mut UiState, query: Query) -> oneshot::Receiver<Reply> {
        let (reply, answer) = oneshot::channel();
        s.open_query(UiQuery { query, reply });
        answer
    }

    #[test]
    fn questions_are_answered_one_at_a_time() {
        let mut s = state();
        let mut answer = open(&mut s, questions());
        assert_eq!(s.query.as_ref().unwrap().title(), "Database · 1/2");
        s.on_key(key(KeyCode::Char('2')));
        assert!(s.query.as_ref().unwrap().is_multi_select());
        s.on_key(key(KeyCode::Char(' ')));
        s.on_key(key(KeyCode::Down));
        s.on_key(key(KeyCode::Char(' ')));
        s.on_key(key(KeyCode::Enter));
        assert_eq!(
            answer.try_recv().unwrap(),
            Reply::Answers(vec![Some("SQLite".into()), Some("Lint, Tests".into())])
        );
        assert!(s.query.is_none());
    }

    #[test]
    fn something_else_can_be_typed_and_esc_skips() {
        let mut s = state();
        let mut answer = open(&mut s, questions());
        s.on_key(key(KeyCode::Char('3')));
        type_text(&mut s, "DuckDB");
        s.on_key(key(KeyCode::Enter));
        s.on_key(key(KeyCode::Esc));
        assert_eq!(
            answer.try_recv().unwrap(),
            Reply::Answers(vec![Some("DuckDB".into()), None])
        );
    }

    #[test]
    fn a_plan_is_approved_with_a_mode_or_sent_back_with_feedback() {
        let mut s = state();
        *s.mode.lock().unwrap() = Mode::Plan;
        let mut answer = open(&mut s, Query::Plan("1. Add the table".into()));
        assert!(
            matches!(&s.flush[..], [Item::Notice(_), Item::Assistant(p)] if p == "1. Add the table")
        );
        let labels: Vec<String> = s
            .query
            .as_ref()
            .unwrap()
            .options()
            .iter()
            .map(|o| o.label().to_string())
            .collect();
        assert_eq!(
            labels[0], "Yes, and accept edits",
            "no auto mode offered without review"
        );
        s.on_key(key(KeyCode::Enter));
        assert_eq!(
            answer.try_recv().unwrap(),
            Reply::PlanApproved(WorkMode::AcceptEdits)
        );
        assert_eq!(s.mode(), Mode::AcceptEdits);

        let mut again = open(&mut s, Query::Plan("2. Revised".into()));
        s.on_key(key(KeyCode::Char('3')));
        type_text(&mut s, "use SQLite");
        s.on_key(key(KeyCode::Enter));
        assert_eq!(
            again.try_recv().unwrap(),
            Reply::KeepPlanning(Some("use SQLite".into()))
        );
    }

    #[test]
    fn a_second_query_waits_for_the_first() {
        let mut s = state();
        let _first = open(&mut s, questions());
        let _second = open(&mut s, Query::Plan("p".into()));
        assert!(matches!(
            s.query.as_ref().unwrap().query,
            Query::Questions(_)
        ));
        s.drop_queries();
        assert!(s.query.is_none() && s.pending_queries.is_empty());
    }
}
