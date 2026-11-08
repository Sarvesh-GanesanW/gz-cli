//! The model, effort, `/resume`, `/autocompact` and `/subagents model` pickers.

use crossterm::event::{KeyCode, KeyEvent};

use crate::assistant::history::{Speaker, Transcript};
use crate::assistant::models::{Model, Selection};
use crate::assistant::subagent_settings::{self, Setting};
use crate::code_ui::items::Item;
use crate::code_ui::resume;
use crate::code_ui::skills::SkillsAction;

use super::{Effect, UiState};

/// One row of the model or effort picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickerRow {
    Header(String),
    Model(Model),
    Effort(String),
    /// An earlier conversation in the `/resume` list.
    Conversation {
        id: String,
        title: String,
        /// Dim text after the title, e.g. `2h ago`.
        detail: String,
    },
    /// An `/autocompact` choice in tokens; `None` is the server default.
    AutoCompact(Option<u64>),
    /// A `/subagents model` choice: a mode value, or `None` for the chat's
    /// own model.
    SubagentModel(Option<String>),
    /// A `/skills` row: enter switches the skill on or off.
    Skill {
        name: String,
        on: bool,
        /// Its description, or why it can't be used.
        detail: String,
        /// A team skill waiting for the user's trust: enter reviews it.
        needs_trust: bool,
    },
}

/// The `/models`, `/effort`, `/resume` and `/autocompact` list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picker {
    pub title: String,
    pub rows: Vec<PickerRow>,
    pub selected: usize,
    /// For the effort step: the model the effort applies to.
    pub model: Option<Model>,
}

impl Picker {
    fn selectable(&self, i: usize) -> bool {
        !matches!(self.rows.get(i), Some(PickerRow::Header(_)) | None)
    }

    fn step(&mut self, down: bool) {
        let mut i = self.selected;
        loop {
            let next = if down { i + 1 } else { i.wrapping_sub(1) };
            if next >= self.rows.len() {
                return;
            }
            i = next;
            if self.selectable(i) {
                self.selected = i;
                return;
            }
        }
    }
}

impl UiState {
    fn current_model_key(&self) -> Option<String> {
        self.model
            .clone()
            .or_else(|| self.catalog.as_ref()?.default_selection())
            .map(|s| s.model.key())
    }

    fn current_effort(&self) -> Option<String> {
        self.model
            .clone()
            .or_else(|| self.catalog.as_ref()?.default_selection())
            .and_then(|s| s.effort)
    }

    pub(super) fn open_model_picker(&mut self) {
        let Some(catalog) = &self.catalog else {
            let message = if self.models_loading {
                "The model list is still loading; try /models again in a moment"
            } else {
                "The model list couldn't be loaded; check `gz doctor`"
            };
            self.record(Item::Error(message.into()));
            return;
        };
        let mut rows = vec![PickerRow::Header("Groundzero".into())];
        rows.extend(
            catalog
                .modes
                .iter()
                .cloned()
                .map(|m| PickerRow::Model(Model::Groundzero(m))),
        );
        if !catalog.connections.is_empty() {
            rows.push(PickerRow::Header("Your connections".into()));
            rows.extend(
                catalog
                    .connections
                    .iter()
                    .cloned()
                    .map(|c| PickerRow::Model(Model::Connection(c))),
            );
        }
        let current = self.current_model_key();
        let selected = rows
            .iter()
            .position(|r| matches!(r, PickerRow::Model(m) if Some(m.key()) == current))
            .unwrap_or(1);
        self.picker = Some(Picker {
            title: "Select model".into(),
            rows,
            selected,
            model: None,
        });
    }

    pub(super) fn open_effort_picker(&mut self, model: Model) {
        let efforts = model.efforts().to_vec();
        if efforts.is_empty() {
            self.choose_model(Selection {
                model,
                effort: None,
            });
            return;
        }
        let current = self.current_effort();
        let rows: Vec<PickerRow> = efforts.into_iter().map(PickerRow::Effort).collect();
        let selected = rows
            .iter()
            .position(|r| matches!(r, PickerRow::Effort(e) if Some(e) == current.as_ref()))
            .unwrap_or(0);
        self.picker = Some(Picker {
            title: format!("Reasoning effort for {}", model.label()),
            rows,
            selected,
            model: Some(model),
        });
    }

    /// Opens the `/resume` picker with `rows`, or says why it can't.
    pub fn show_conversations(&mut self, rows: Result<Vec<PickerRow>, String>) {
        match rows {
            Err(err) => self.record(Item::Error(format!(
                "cannot load your conversations: {}",
                crate::http::readable_error(&err)
            ))),
            Ok(rows) => match rows
                .iter()
                .position(|r| matches!(r, PickerRow::Conversation { .. }))
            {
                None => self.record(Item::Notice("No earlier conversations yet".into())),
                Some(selected) => {
                    self.picker = Some(Picker {
                        title: "Resume a conversation".into(),
                        rows,
                        selected,
                        model: None,
                    })
                }
            },
        }
    }

    /// Prints a resumed conversation's earlier messages.
    pub fn show_history(&mut self, title: &str, transcript: Result<Transcript, String>) {
        match transcript {
            Ok(transcript) => {
                let title = transcript.title.as_deref().unwrap_or(title);
                for item in resume::history_items(title, &transcript.messages) {
                    self.record(item);
                }
                // Auto mode judges actions by what the user asked, earlier
                // turns included.
                self.with_auto(|auto| {
                    for message in &transcript.messages {
                        if message.speaker == Speaker::You {
                            auto.user_said(&message.text);
                        }
                    }
                });
            }
            Err(err) => self.record(Item::Error(format!(
                "Continuing “{title}”, but its earlier messages couldn't be loaded: {}",
                crate::http::readable_error(&err)
            ))),
        }
    }

    fn choose_model(&mut self, selection: Selection) {
        self.record(Item::Notice(format!("Model: {}", selection.label())));
        self.model = Some(selection);
        self.picker = None;
    }

    pub(super) fn on_picker_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let Some(picker) = self.picker.as_mut() else {
            return Vec::new();
        };
        match key.code {
            KeyCode::Esc => self.picker = None,
            KeyCode::Up | KeyCode::Char('k') => picker.step(false),
            KeyCode::Down | KeyCode::Char('j') => picker.step(true),
            KeyCode::Enter | KeyCode::Char(' ')
                if matches!(
                    picker.rows.get(picker.selected),
                    Some(PickerRow::Skill { .. })
                ) =>
            {
                return toggle_skill(picker);
            }
            KeyCode::Enter => match picker.rows.get(picker.selected).cloned() {
                Some(PickerRow::Conversation { id, title, .. }) => {
                    self.picker = None;
                    let mut effects = self.leave_conversation();
                    self.record(Item::Notice(format!("Loading “{title}”…")));
                    effects.push(Effect::Resume { id, title });
                    return effects;
                }
                Some(PickerRow::AutoCompact(tokens)) => return self.choose_auto_compact(tokens),
                Some(PickerRow::SubagentModel(mode)) => {
                    let value = mode.as_deref().unwrap_or(subagent_settings::MAIN_WORD);
                    return self.set_subagent_setting(Setting::Model, value);
                }
                Some(PickerRow::Model(model)) => self.open_effort_picker(model),
                Some(PickerRow::Effort(effort)) => {
                    if let Some(model) = picker.model.clone() {
                        self.choose_model(Selection {
                            model,
                            effort: Some(effort),
                        });
                    }
                }
                _ => {}
            },
            _ => {}
        }
        match (&self.picker, &self.model) {
            (None, Some(selection)) if key.code == KeyCode::Enter => {
                vec![Effect::SetModel(selection.clone())]
            }
            _ => Vec::new(),
        }
    }
}

/// Flips the highlighted skill in the list (which stays open) and asks the
/// driver to save it; a team skill waiting for trust opens its review.
fn toggle_skill(picker: &mut Picker) -> Vec<Effect> {
    let Some(PickerRow::Skill {
        name,
        on,
        needs_trust,
        ..
    }) = picker.rows.get_mut(picker.selected)
    else {
        return Vec::new();
    };
    if *needs_trust {
        return vec![Effect::Skills(SkillsAction::Trust(name.clone()))];
    }
    *on = !*on;
    vec![Effect::Skills(SkillsAction::SetEnabled {
        name: name.clone(),
        on: *on,
    })]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::code_ui::state::test_support::*;

    use crossterm::event::KeyCode;

    use crate::assistant::history::Transcript;

    use crate::code_ui::items::Item;

    #[test]
    fn model_picker_marks_current_and_picks_mode_then_effort() {
        let mut s = with_catalog();
        assert_eq!(s.model_label().as_deref(), Some("Claude Sonnet 5.5"));
        type_text(&mut s, "/models");
        s.on_key(key(KeyCode::Enter));
        let picker = s.picker.as_ref().unwrap();
        assert_eq!(picker.rows[0], PickerRow::Header("Groundzero".into()));
        assert!(
            matches!(&picker.rows[picker.selected], PickerRow::Model(m) if m.key() == "sonnet55"),
            "current is preselected"
        );
        assert!(picker
            .rows
            .contains(&PickerRow::Header("Your connections".into())));
        s.on_key(key(KeyCode::Up));
        s.on_key(key(KeyCode::Up));
        assert_eq!(
            s.picker.as_ref().unwrap().selected,
            1,
            "headers are skipped"
        );
        assert!(
            s.on_key(key(KeyCode::Enter)).is_empty(),
            "opus has efforts: next step"
        );
        assert!(s.picker.as_ref().unwrap().title.contains("effort"));
        s.on_key(key(KeyCode::Down));
        let effects = s.on_key(key(KeyCode::Enter));
        let Effect::SetModel(selection) = &effects[0] else {
            panic!("{effects:?}")
        };
        assert_eq!(selection.label(), "Claude Opus 5 · high");
        assert_eq!(s.model_label().as_deref(), Some("Claude Opus 5 · high"));
    }

    #[test]
    fn connections_skip_the_effort_step_and_esc_cancels() {
        let mut s = with_catalog();
        type_text(&mut s, "/models");
        s.on_key(key(KeyCode::Enter));
        s.on_key(key(KeyCode::Esc));
        assert!(s.picker.is_none());
        type_text(&mut s, "/models");
        s.on_key(key(KeyCode::Enter));
        for _ in 0..5 {
            s.on_key(key(KeyCode::Down));
        }
        let effects = s.on_key(key(KeyCode::Enter));
        assert!(
            matches!(&effects[0], Effect::SetModel(sel) if sel.model.key() == "conn:OPENAI:my-gpt")
        );
    }

    #[test]
    fn resume_lists_conversations_and_continues_the_chosen_one() {
        let mut s = state();
        type_text(&mut s, "/resume");
        assert_eq!(
            s.on_key(key(KeyCode::Enter)),
            vec![Effect::ListConversations]
        );
        s.show_conversations(Ok(vec![
            PickerRow::Header("This folder".into()),
            PickerRow::Conversation {
                id: "c1".into(),
                title: "Jobs".into(),
                detail: "2h ago".into(),
            },
            PickerRow::Conversation {
                id: "c2".into(),
                title: "Charts".into(),
                detail: String::new(),
            },
        ]));
        assert_eq!(s.picker.as_ref().unwrap().selected, 1);
        s.on_key(key(KeyCode::Down));
        assert_eq!(
            s.on_key(key(KeyCode::Enter)),
            vec![Effect::Resume {
                id: "c2".into(),
                title: "Charts".into()
            }]
        );
        assert!(s.picker.is_none());
        s.show_history(
            "Charts",
            Ok(Transcript {
                title: Some("Creating charts".into()),
                messages: vec![crate::assistant::history::Message {
                    speaker: crate::assistant::history::Speaker::You,
                    text: "make a bar chart".into(),
                }],
            }),
        );
        assert!(s
            .flush
            .iter()
            .any(|i| matches!(i, Item::Notice(n) if n.contains("Creating charts"))));
        assert!(matches!(s.flush.last(), Some(Item::User(t)) if t == "make a bar chart"));
    }

    #[test]
    fn resume_with_nothing_to_list_says_so() {
        let mut s = state();
        s.show_conversations(Ok(Vec::new()));
        assert!(s.picker.is_none());
        assert!(matches!(s.flush.last(), Some(Item::Notice(n)) if n.contains("No earlier")));
        s.show_conversations(Err("POST https://x → HTTP 500: boom".into()));
        assert!(matches!(s.flush.last(), Some(Item::Error(e)) if e.ends_with("HTTP 500: boom")));
    }
}
