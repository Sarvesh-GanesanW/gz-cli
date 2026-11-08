//! `/compact [focus]` and `/autocompact [tokens|default]`: compacting the
//! conversation now, and choosing when the chat does it by itself.

use crate::assistant::compaction::{self, Report};
use crate::code_ui::items::Item;
use crate::code_ui::panel::{Panel, Row};
use crate::code_ui::usage;

use super::{Effect, Picker, PickerRow, UiState};

impl UiState {
    /// `/compact [instructions]`: refused while an answer is running.
    pub(super) fn compact_command(&mut self, command: &str) -> Vec<Effect> {
        if self.is_busy() || self.compacting {
            self.record(Item::Error(
                "Wait for the answer to finish (or esc) before compacting".into(),
            ));
            return Vec::new();
        }
        let focus = command
            .trim()
            .strip_prefix("/compact")
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string);
        self.compacting = true;
        vec![Effect::Compact(focus)]
    }

    /// The `/compact` card, then whatever was typed while it ran.
    pub fn show_compaction(&mut self, result: Result<Report, String>) -> Vec<Effect> {
        self.compacting = false;
        match result {
            Ok(report) if report.compacted => {
                self.record(Item::Panel(compaction_card(&report)));
                // The meter showed the old size; the next run reports the new one.
                self.usage.context = None;
            }
            Ok(_) => self.record(Item::Notice(
                "Nothing to compact yet: only the latest turns are in the history".into(),
            )),
            Err(why) if why.contains("HTTP 404") => self.record(Item::Notice(
                "Nothing to compact yet: this conversation has no saved turns".into(),
            )),
            Err(why) => self.record(Item::Error(format!(
                "couldn't compact the conversation: {}",
                crate::http::readable_error(&why)
            ))),
        }
        match self.queue.pop_front() {
            Some(next) => vec![Effect::Send(next)],
            None => Vec::new(),
        }
    }

    /// `/autocompact` opens the picker; `/autocompact 300k|default` sets it.
    pub(super) fn auto_compact_command(&mut self, command: &str) -> Vec<Effect> {
        let Some(value) = command.split_whitespace().nth(1) else {
            self.open_auto_compact_picker();
            return Vec::new();
        };
        match compaction::parse(value) {
            Ok(tokens) => self.choose_auto_compact(tokens),
            Err(err) => {
                self.record(Item::Error(format!("{err}")));
                Vec::new()
            }
        }
    }

    pub(super) fn choose_auto_compact(&mut self, tokens: Option<u64>) -> Vec<Effect> {
        self.auto_compact = tokens;
        self.picker = None;
        let note = match tokens {
            Some(tokens) => format!(
                "Auto-compact at {} tokens (capped at the model's window)",
                compaction::label(Some(tokens))
            ),
            None => "Auto-compact at the server default (85% of the model's window)".into(),
        };
        self.record(Item::Notice(note));
        vec![Effect::SetAutoCompact(tokens)]
    }

    /// The current model's window: the model list's figure, else the last
    /// run's report.
    fn context_window(&self) -> Option<u64> {
        self.model
            .clone()
            .or_else(|| self.catalog.as_ref()?.default_selection())
            .and_then(|selection| selection.model.context_window())
            .or_else(|| self.usage.context.as_ref().map(|c| c.window))
    }

    fn open_auto_compact_picker(&mut self) {
        let mut rows = vec![PickerRow::AutoCompact(None)];
        rows.extend(
            compaction::steps(self.context_window())
                .into_iter()
                .map(|tokens| PickerRow::AutoCompact(Some(tokens))),
        );
        let selected = rows
            .iter()
            .position(|row| *row == PickerRow::AutoCompact(self.auto_compact))
            .unwrap_or(0);
        self.picker = Some(Picker {
            title: "Auto-compact when the conversation reaches".into(),
            rows,
            selected,
            model: None,
        });
    }
}

/// `Conversation compacted · 40 messages summarized`, with the size of the
/// history the next turn resends.
fn compaction_card(report: &Report) -> Panel {
    Panel::new("Conversation compacted")
        .aside(format!(
            "{} messages summarized",
            report.summarized_messages
        ))
        .section(
            None,
            vec![Row::field(
                "history",
                format!(
                    "~{} → ~{} tokens resent per turn",
                    usage::short(report.before_tokens),
                    usage::short(report.after_tokens)
                ),
            )],
        )
        .footer("Later turns send a summary of the earlier conversation plus the latest turns")
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyCode;

    use super::*;
    use crate::code_ui::state::test_support::*;

    fn report(compacted: bool) -> Report {
        Report {
            compacted,
            summarized_messages: 40,
            before_tokens: 61_234,
            after_tokens: 3_120,
        }
    }

    #[test]
    fn compact_sends_the_focus_and_shows_the_card() {
        let mut s = state();
        s.usage.context = Some(crate::cmd::stream::ContextUsage {
            tokens: Some(150_000),
            window: 200_000,
            auto_compact_at: 170_000,
            history: None,
        });
        type_text(&mut s, "/compact keep the schema decisions");
        assert_eq!(
            s.on_key(key(KeyCode::Enter)),
            vec![Effect::Compact(Some("keep the schema decisions".into()))]
        );
        type_text(&mut s, "next question");
        assert!(s.on_key(key(KeyCode::Enter)).is_empty(), "queued meanwhile");
        let after = s.show_compaction(Ok(report(true)));
        assert_eq!(after, vec![Effect::Send("next question".into())]);
        let Some(Item::Panel(card)) = s.flush.last() else {
            panic!("no card: {:?}", s.flush);
        };
        let lines = card.plain_lines().join("\n");
        assert!(
            lines.contains("Conversation compacted · 40 messages summarized"),
            "{lines}"
        );
        assert!(lines.contains("~61.2k → ~3.1k tokens"), "{lines}");
        assert_eq!(s.usage.context, None, "the meter starts over");
        assert!(!s.compacting);
    }

    #[test]
    fn compact_without_focus_and_with_nothing_to_do() {
        let mut s = state();
        type_text(&mut s, "/compact");
        assert_eq!(s.on_key(key(KeyCode::Enter)), vec![Effect::Compact(None)]);
        s.show_compaction(Ok(report(false)));
        assert!(
            matches!(s.flush.last(), Some(Item::Notice(n)) if n.contains("Nothing to compact"))
        );
        s.compacting = true;
        s.show_compaction(Err(
            "POST https://x → HTTP 404: Conversation not found".into()
        ));
        assert!(matches!(s.flush.last(), Some(Item::Notice(n)) if n.contains("no saved turns")));
        s.show_compaction(Err("POST https://x → HTTP 502: down".into()));
        assert!(matches!(s.flush.last(), Some(Item::Error(e)) if e.contains("couldn't compact")));
    }

    #[test]
    fn compact_waits_for_a_running_answer() {
        let mut s = state();
        s.start_turn("hi");
        type_text(&mut s, "/compact");
        assert!(s.on_key(key(KeyCode::Enter)).is_empty());
        assert!(matches!(s.flush.last(), Some(Item::Error(e)) if e.contains("Wait")));
        assert!(!s.compacting);
    }

    #[test]
    fn autocompact_sets_tokens_or_the_default() {
        let mut s = state();
        type_text(&mut s, "/autocompact 300k");
        assert_eq!(
            s.on_key(key(KeyCode::Enter)),
            vec![Effect::SetAutoCompact(Some(300_000))]
        );
        assert_eq!(s.auto_compact, Some(300_000));
        type_text(&mut s, "/autocompact default");
        assert_eq!(
            s.on_key(key(KeyCode::Enter)),
            vec![Effect::SetAutoCompact(None)]
        );
        type_text(&mut s, "/autocompact 150k");
        assert!(s.on_key(key(KeyCode::Enter)).is_empty());
        assert!(matches!(s.flush.last(), Some(Item::Error(e)) if e.contains("100k steps")));
        assert_eq!(s.auto_compact, None, "a bad value changes nothing");
    }

    #[test]
    fn autocompact_picker_offers_steps_up_to_the_models_window() {
        let mut s = with_catalog();
        s.auto_compact = Some(200_000);
        type_text(&mut s, "/autocompact");
        assert!(s.on_key(key(KeyCode::Enter)).is_empty());
        let picker = s.picker.as_ref().unwrap();
        assert_eq!(picker.rows.first(), Some(&PickerRow::AutoCompact(None)));
        assert_eq!(
            picker.rows.last(),
            Some(&PickerRow::AutoCompact(Some(200_000))),
            "the default model's window is 200k"
        );
        assert_eq!(
            picker.rows[picker.selected],
            PickerRow::AutoCompact(Some(200_000))
        );
        s.on_key(key(KeyCode::Up));
        assert_eq!(
            s.on_key(key(KeyCode::Enter)),
            vec![Effect::SetAutoCompact(Some(100_000))]
        );
        assert!(s.picker.is_none());
        s.model = s.catalog.as_ref().unwrap().resolve("opus", None);
        type_text(&mut s, "/autocompact");
        s.on_key(key(KeyCode::Enter));
        assert_eq!(
            s.picker.as_ref().unwrap().rows.last(),
            Some(&PickerRow::AutoCompact(Some(800_000))),
            "an 872k window offers up to 800k"
        );
    }
}
