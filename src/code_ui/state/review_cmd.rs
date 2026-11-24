//! `/review [target]` and `/security-review [target]`: send the review
//! request as a normal turn.

use crate::assistant::turn_marks::CommandMark;
use crate::code_ui::items::Item;
use crate::code_ui::review::{self, Focus, Target};

use super::{Effect, UiState};

impl UiState {
    pub(super) fn review_command(&mut self, command: &str, focus: Focus) -> Vec<Effect> {
        let argument = command
            .split_once(char::is_whitespace)
            .map_or("", |(_, rest)| rest);
        let prompt = Target::parse(argument)
            .and_then(|target| review::prompt(focus, &target, review::has_gh()));
        let prompt = match prompt {
            Ok(prompt) => CommandMark::wrap(command, &prompt),
            Err(why) => {
                self.record(Item::Error(why));
                return Vec::new();
            }
        };
        if self.is_busy() || self.compacting {
            // A review is its own turn: it waits for the running one.
            self.queue.push_back(command.to_string());
            self.record(Item::Notice(format!(
                "{} starts when the current answer ends",
                focus.command()
            )));
            return Vec::new();
        }
        vec![Effect::Send(prompt)]
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyCode;

    use super::*;
    use crate::code_ui::state::test_support::*;

    #[test]
    fn review_sends_the_canned_request_and_bad_targets_say_why() {
        let mut s = state();
        type_text(&mut s, "/review feature/login");
        let effects = s.on_key(key(KeyCode::Enter));
        let [Effect::Send(prompt)] = effects.as_slice() else {
            panic!("{effects:?}");
        };
        let (typed, expansion) = CommandMark::split(prompt).expect("marked");
        assert_eq!(typed, "/review feature/login");
        assert!(
            expansion.starts_with("Review the branch `feature/login`"),
            "{prompt}"
        );

        type_text(&mut s, "/security-review");
        let effects = s.on_key(key(KeyCode::Enter));
        let [Effect::Send(prompt)] = effects.as_slice() else {
            panic!("{effects:?}");
        };
        let expansion = CommandMark::split(prompt).expect("marked").1;
        assert!(expansion.starts_with("Do a security review of the uncommitted changes"));

        type_text(&mut s, "/review a b");
        assert!(s.on_key(key(KeyCode::Enter)).is_empty());
        assert!(
            matches!(s.flush.last(), Some(Item::Error(e)) if e.contains("one thing at a time"))
        );
    }
}
