//! Permission prompts: how a pending tool approval is shown and answered.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::assistant::tools::Access;
use crate::assistant::{Decision, PermissionRequest};
use crate::code_ui::approver::{Answer, Ask, Mode};
use crate::code_ui::items::Item;

use super::UiState;

/// A permission prompt on screen.
pub struct PromptView {
    pub request: PermissionRequest,
    pub selected: usize,
    /// `Some` while typing the "tell gz what to do differently" note.
    pub note: Option<String>,
    reply: Option<tokio::sync::oneshot::Sender<Answer>>,
    /// The background subagent asking, if one is.
    background: Option<String>,
    /// Offer "Yes, and switch to auto mode": the mode asks and the server
    /// reviews actions.
    offer_auto: bool,
}

/// One answer a permission prompt offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    Once,
    Chat,
    Always,
    /// Allow this one and let auto mode decide from now on.
    SwitchToAuto,
    /// "No, and tell gz what to do differently": opens the note field.
    Deny,
}

impl PromptView {
    #[cfg(test)]
    pub fn for_test(request: PermissionRequest) -> Self {
        Self {
            request,
            selected: 0,
            note: None,
            reply: None,
            background: None,
            offer_auto: true,
        }
    }

    /// The answers on offer, in order; "always" only when the request can
    /// be remembered forever.
    pub fn options(&self) -> Vec<(Choice, String)> {
        if self.request.access == Access::Folder {
            return self.folder_options();
        }
        if self.request.access == Access::Install {
            return vec![
                (Choice::Once, "Yes, install".to_string()),
                (Choice::Deny, "No (esc)".to_string()),
            ];
        }
        let mut options = vec![
            (Choice::Once, "Yes".to_string()),
            (
                Choice::Chat,
                format!("Yes, for this chat ({})", self.request.chat_scope),
            ),
        ];
        if let Some(scope) = &self.request.always_scope {
            options.push((Choice::Always, format!("Yes, always ({scope})")));
        }
        if self.offer_auto {
            options.push((
                Choice::SwitchToAuto,
                "Yes, and switch to auto mode".to_string(),
            ));
        }
        options.push((
            Choice::Deny,
            "No, and tell gz what to do differently (esc)".to_string(),
        ));
        options
    }
}

impl PromptView {
    /// A folder outside the open ones: for this session, always, or no.
    fn folder_options(&self) -> Vec<(Choice, String)> {
        let mut options = vec![(Choice::Chat, "Yes, for this session".to_string())];
        if let Some(scope) = &self.request.always_scope {
            options.push((Choice::Always, format!("Yes, and {scope}")));
        }
        options.push((
            Choice::Deny,
            "No, and tell gz what to do differently (esc)".to_string(),
        ));
        options
    }
}

/// How a prompt was answered, for the transcript.
fn decision_item(request: &PermissionRequest, answer: &Answer) -> Item {
    let choice = match answer {
        (Decision::AllowOnce, _) => "allowed once".to_string(),
        (Decision::AllowChat, _) => format!("allowed for this chat: {}", request.chat_scope),
        (Decision::AllowAlways, _) => format!(
            "allowed always: {}",
            request
                .always_scope
                .as_deref()
                .unwrap_or(&request.chat_scope)
        ),
        (Decision::Deny, Some(note)) if !note.is_empty() => format!("denied: {note}"),
        (Decision::Deny, _) => "denied".to_string(),
    };
    Item::Decision {
        title: request.summary.clone(),
        choice,
        preview: request.preview.clone(),
    }
}

impl UiState {
    /// Shows `ask`, or queues it behind the prompt already open.
    pub fn open_prompt(&mut self, ask: Ask) {
        if self.prompt.is_some() {
            self.pending_prompts.push_back(ask);
            return;
        }
        // Prompts are answered on the main page.
        self.agent_view = None;
        self.close_text();
        self.close_group();
        self.prompt = Some(PromptView {
            request: ask.request,
            selected: 0,
            note: None,
            reply: Some(ask.reply),
            background: ask.background,
            offer_auto: self.mode().is_manual() && self.auto_available(),
        });
    }

    pub(super) fn answer(&mut self, answer: Answer) {
        let Some(mut prompt) = self.prompt.take() else {
            return;
        };
        self.transcript
            .push(decision_item(&prompt.request, &answer));
        if let Some(reply) = prompt.reply.take() {
            reply.send(answer).ok();
        }
        self.open_next_prompt();
    }

    /// The next waiting prompt, or back to the input with what was typed
    /// meanwhile.
    fn open_next_prompt(&mut self) {
        match self.pending_prompts.pop_front() {
            Some(next) => self.open_prompt(next),
            None => self.restore_typed_ahead(),
        }
    }

    /// Drops the prompts whose asker matches `whose` (by background
    /// subagent id; `None` is the main agent). Dropping one refuses it.
    pub(super) fn drop_prompts(&mut self, whose: impl Fn(Option<&str>) -> bool) {
        self.pending_prompts
            .retain(|ask| !whose(ask.background.as_deref()));
        if self
            .prompt
            .as_ref()
            .is_some_and(|p| whose(p.background.as_deref()))
        {
            self.prompt = None;
            self.open_next_prompt();
        }
    }

    pub(super) fn restore_typed_ahead(&mut self) {
        let typed = std::mem::take(&mut self.typed_ahead);
        if !typed.is_empty() {
            self.input.insert_str(&typed);
        }
    }

    /// Acts on the prompt's option at `index` (ignored when out of range).
    fn choose(&mut self, index: usize) {
        let Some(prompt) = self.prompt.as_mut() else {
            return;
        };
        let options = prompt.options();
        let Some((choice, _)) = options.get(index) else {
            return;
        };
        match choice {
            Choice::Once => self.answer((Decision::AllowOnce, None)),
            Choice::Chat => self.answer((Decision::AllowChat, None)),
            Choice::Always => self.answer((Decision::AllowAlways, None)),
            Choice::SwitchToAuto => {
                self.set_auto(true);
                self.answer((Decision::AllowOnce, None));
            }
            Choice::Deny if prompt.request.access == Access::Install => {
                self.answer((Decision::Deny, None))
            }
            Choice::Deny => {
                prompt.selected = index;
                prompt.note = Some(String::new());
            }
        }
    }

    pub(super) fn on_prompt_key(&mut self, key: KeyEvent) {
        let Some(prompt) = self.prompt.as_mut() else {
            return;
        };
        if let Some(note) = prompt.note.as_mut() {
            match key.code {
                KeyCode::Enter => {
                    let note = std::mem::take(note);
                    self.answer((Decision::Deny, Some(note)));
                }
                KeyCode::Esc => self.answer((Decision::Deny, None)),
                KeyCode::Backspace => {
                    note.pop();
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => note.push(c),
                _ => {}
            }
            return;
        }
        let last = prompt.options().len() - 1;
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => prompt.selected = prompt.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => prompt.selected = (prompt.selected + 1).min(last),
            KeyCode::Enter => {
                let selected = prompt.selected;
                self.choose(selected);
            }
            KeyCode::Char(c @ '1'..='9') => self.choose(c as usize - '1' as usize),
            KeyCode::Char('y') => self.choose(0),
            KeyCode::Char('n') | KeyCode::Esc => self.choose(last),
            KeyCode::BackTab if prompt.request.access == Access::Write => {
                if let Ok(mut mode) = self.mode.lock() {
                    *mode = Mode::AcceptEdits;
                }
                self.answer((Decision::AllowChat, None));
            }
            _ => {}
        }
    }

    /// A printable key the open prompt doesn't use: kept for the input box
    /// instead of being lost.
    pub(super) fn is_typed_ahead(&self, key: KeyEvent) -> bool {
        let Some(prompt) = &self.prompt else {
            return false;
        };
        let KeyCode::Char(c) = key.code else {
            return false;
        };
        let plain = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        let prompt_key = matches!(c, 'y' | 'n' | 'j' | 'k' | '1'..='9');
        plain && prompt.note.is_none() && !prompt_key
    }
}

#[cfg(test)]
mod tests {

    use crate::code_ui::state::test_support::*;

    use crossterm::event::KeyCode;

    use crate::assistant::tools::Access;
    use crate::assistant::{Decision, PermissionRequest};

    use crate::code_ui::approver::{Ask, Mode};

    use crate::code_ui::items::Item;

    #[test]
    fn prompts_choose_by_arrow_number_or_note() {
        let mut s = state();
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        s.open_prompt(Ask {
            request: PermissionRequest {
                access: Access::Write,
                summary: "edit_file a.rs".into(),
                preview: None,
                chat_scope: "all edits this session".into(),
                always_scope: Some("always scope".into()),
                action: None,
                asked_by: None,
                target: None,
                note: None,
            },
            reply: tx,
            background: None,
        });
        for _ in 0..3 {
            s.on_key(key(KeyCode::Down));
        }
        s.on_key(key(KeyCode::Enter));
        assert_eq!(
            s.prompt.as_ref().unwrap().note.as_deref(),
            Some(""),
            "option 4 asks for a note"
        );
        type_text(&mut s, "use a loop");
        s.on_key(key(KeyCode::Enter));
        assert!(s.prompt.is_none());
        assert_eq!(
            rx.try_recv().unwrap(),
            (Decision::Deny, Some("use a loop".into()))
        );

        let (tx, mut rx) = tokio::sync::oneshot::channel();
        s.open_prompt(Ask {
            request: PermissionRequest {
                access: Access::Command,
                summary: "run_command ls".into(),
                preview: Some("ls".into()),
                chat_scope: "this exact command".into(),
                always_scope: Some("always scope".into()),
                action: None,
                asked_by: None,
                target: None,
                note: None,
            },
            reply: tx,
            background: None,
        });
        s.on_key(key(KeyCode::Char('2')));
        assert_eq!(rx.try_recv().unwrap(), (Decision::AllowChat, None));

        let (tx, mut rx) = tokio::sync::oneshot::channel();
        s.open_prompt(Ask {
            request: PermissionRequest {
                access: Access::Command,
                summary: "run_command ls".into(),
                preview: Some("ls".into()),
                chat_scope: "this exact command".into(),
                always_scope: Some("this exact command in this folder".into()),
                action: None,
                asked_by: None,
                target: None,
                note: None,
            },
            reply: tx,
            background: None,
        });
        s.on_key(key(KeyCode::Char('3')));
        assert_eq!(rx.try_recv().unwrap(), (Decision::AllowAlways, None));
    }

    #[test]
    fn without_always_the_last_option_is_still_deny() {
        let mut s = state();
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        s.open_prompt(Ask {
            request: PermissionRequest {
                access: Access::ServerAction,
                summary: "Delete dataset".into(),
                preview: None,
                chat_scope: "this action in this chat".into(),
                always_scope: None,
                action: Some("delete_dataset".into()),
                asked_by: None,
                target: None,
                note: None,
            },
            reply: tx,
            background: None,
        });
        assert_eq!(s.prompt.as_ref().unwrap().options().len(), 3);
        s.on_key(key(KeyCode::Char('4')));
        assert!(
            s.prompt.is_some(),
            "4 does nothing when there are three options"
        );
        s.on_key(key(KeyCode::Char('n')));
        s.on_key(key(KeyCode::Esc));
        assert_eq!(rx.try_recv().unwrap(), (Decision::Deny, None));
    }

    fn auto_state() -> crate::code_ui::state::UiState {
        let mut s = with_catalog();
        s.auto = Some(std::sync::Arc::new(std::sync::Mutex::new(
            crate::code_ui::auto_mode::AutoContext::default(),
        )));
        s
    }

    fn labels(s: &crate::code_ui::state::UiState) -> Vec<String> {
        let prompt = s.prompt.as_ref().unwrap();
        prompt
            .options()
            .into_iter()
            .map(|(_, label)| label)
            .collect()
    }

    #[test]
    fn a_manual_prompt_offers_to_switch_to_auto_mode() {
        let mut s = auto_state();
        let (ask, mut rx) = ask(Access::Command, "run cargo test", None);
        s.open_prompt(ask);
        let options = labels(&s);
        assert_eq!(options[3], "Yes, and switch to auto mode");
        assert!(options[4].starts_with("No"), "deny stays last");
        s.on_key(key(KeyCode::Char('4')));
        assert_eq!(rx.try_recv().unwrap(), (Decision::AllowOnce, None));
        assert_eq!(s.mode(), Mode::Auto);
        assert!(matches!(s.flush.last(), Some(Item::Notice(n)) if n.starts_with("Auto mode on")));
    }

    #[test]
    fn the_switch_is_offered_only_when_auto_mode_could_take_over() {
        let mut plain = state();
        plain.open_prompt(ask(Access::Command, "run ls", None).0);
        assert!(
            !labels(&plain).iter().any(|l| l.contains("auto mode")),
            "no review"
        );
        let mut planning = auto_state();
        *planning.mode.lock().unwrap() = Mode::Plan;
        planning.open_prompt(ask(Access::Read, "read a.rs", None).0);
        assert!(
            !labels(&planning).iter().any(|l| l.contains("auto mode")),
            "plan mode"
        );
    }

    #[test]
    fn shift_tab_cycles_modes_and_on_an_edit_prompt_accepts_edits() {
        let mut s = state();
        s.on_key(key(KeyCode::BackTab));
        assert_eq!(s.mode(), Mode::AcceptEdits);
        s.on_key(key(KeyCode::BackTab));
        assert_eq!(s.mode(), Mode::Plan);
        s.on_key(key(KeyCode::BackTab));
        assert_eq!(s.mode(), Mode::Ask);
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        s.open_prompt(Ask {
            request: PermissionRequest {
                access: Access::Write,
                summary: "edit_file a.rs".into(),
                preview: None,
                chat_scope: "all edits this session".into(),
                always_scope: Some("always scope".into()),
                action: None,
                asked_by: None,
                target: None,
                note: None,
            },
            reply: tx,
            background: None,
        });
        s.on_key(key(KeyCode::BackTab));
        assert_eq!(rx.try_recv().unwrap().0, Decision::AllowChat);
        assert_eq!(s.mode(), Mode::AcceptEdits);
    }

    #[test]
    fn keys_typed_during_a_prompt_land_in_the_input_after_it() {
        let mut s = state();
        let (prompt, mut answer) = ask(Access::Write, "edit_file a.rs", None);
        s.open_prompt(prompt);
        type_text(&mut s, "also ");
        assert!(s.prompt.is_some(), "typing doesn't answer the prompt");
        s.on_key(key(KeyCode::Char('y')));
        assert_eq!(answer.try_recv().unwrap(), (Decision::AllowOnce, None));
        assert_eq!(s.input_text(), "also ");
    }

    #[test]
    fn decisions_are_kept_for_the_transcript() {
        let mut s = state();
        let (prompt, _answer) = ask(
            Access::Script,
            "run_script python (2 lines)",
            Some("import gz\nprint(1)"),
        );
        s.open_prompt(prompt);
        s.on_key(key(KeyCode::Char('1')));
        assert!(matches!(
            s.transcript.last(),
            Some(Item::Decision { choice, preview: Some(code), .. })
                if choice == "allowed once" && code.contains("import gz")
        ));
    }
}
