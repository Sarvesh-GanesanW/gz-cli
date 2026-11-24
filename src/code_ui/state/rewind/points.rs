//! The points `/rewind` can go back to: the user's messages in this
//! conversation, newest first, joined with gz's edit history. A message
//! can rewind the conversation; one gz changed files after can also rewind
//! code. Edit-history turns that match no message on screen (a subagent's
//! report, a finished command, a message older than the reprinted history)
//! stay code-only points.

use crate::assistant::checkpoints::{excerpt, MessageEdits};

/// One place `/rewind` can go back to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewindPoint {
    pub excerpt: String,
    /// What the user typed, when this is one of their messages: the
    /// conversation can be rewound to before it.
    pub prompt: Option<String>,
    /// How many later messages of the user's say exactly the same.
    pub later_repeats: usize,
    /// The edit-history turn a code rewind goes back to: the oldest turn
    /// at or after this point; `None` when gz changed no files since.
    pub code_turn: Option<u64>,
    /// Files gz changed answering this message itself.
    pub files: usize,
    /// When it was sent (Unix ms), when the edit history knows.
    pub at: Option<u64>,
}

/// The points, newest first. `messages` are the typed texts of the user's
/// messages on screen, oldest first; `turns` the edit history's.
pub fn rewind_points(messages: &[String], turns: &[MessageEdits]) -> Vec<RewindPoint> {
    let newest_messages: Vec<&String> = messages.iter().rev().collect();
    let mut newest_turns: Vec<&MessageEdits> = turns.iter().collect();
    newest_turns.sort_by(|a, b| b.turn.cmp(&a.turn));

    let mut points = Vec::new();
    let mut next_message = 0;
    for turn in newest_turns {
        let matched = (next_message..newest_messages.len())
            .find(|&k| excerpt(newest_messages[k]) == turn.excerpt);
        match matched {
            Some(k) => {
                points.extend((next_message..k).map(|i| message_point(i, &newest_messages)));
                let mut point = message_point(k, &newest_messages);
                point.files = turn.files;
                point.at = Some(turn.at);
                point.code_turn = Some(turn.turn);
                points.push(point);
                next_message = k + 1;
            }
            None => points.push(RewindPoint {
                excerpt: turn.excerpt.clone(),
                prompt: None,
                later_repeats: 0,
                code_turn: Some(turn.turn),
                files: turn.files,
                at: Some(turn.at),
            }),
        }
    }
    points
        .extend((next_message..newest_messages.len()).map(|i| message_point(i, &newest_messages)));
    carry_code_turns(&mut points);
    points
}

/// A rewind to a point puts back everything changed since: the oldest
/// turn at or after it.
fn carry_code_turns(points: &mut [RewindPoint]) {
    let mut oldest: Option<u64> = None;
    for point in points.iter_mut() {
        oldest = match (oldest, point.code_turn) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        point.code_turn = oldest;
    }
}

/// The point for the `index`-th newest message.
fn message_point(index: usize, newest_messages: &[&String]) -> RewindPoint {
    let text = newest_messages[index];
    let later_repeats = newest_messages[..index]
        .iter()
        .filter(|m| m.trim() == text.trim())
        .count();
    RewindPoint {
        excerpt: excerpt(text),
        prompt: Some(text.to_string()),
        later_repeats,
        code_turn: None,
        files: 0,
        at: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(id: u64, text: &str, files: usize) -> MessageEdits {
        MessageEdits {
            turn: id,
            excerpt: excerpt(text),
            at: id * 1000,
            files,
        }
    }

    fn said(texts: &[&str]) -> Vec<String> {
        texts.iter().map(|t| t.to_string()).collect()
    }

    #[test]
    fn every_message_is_a_point_and_code_goes_back_to_the_oldest_turn_since() {
        let messages = said(&["plan it", "edit a", "just asking", "edit b"]);
        let turns = [turn(1, "edit a", 1), turn(2, "edit b", 2)];
        let points = rewind_points(&messages, &turns);
        let summary: Vec<(String, Option<u64>, usize)> = points
            .iter()
            .map(|p| (p.excerpt.clone(), p.code_turn, p.files))
            .collect();
        assert_eq!(
            summary,
            [
                ("edit b".into(), Some(2), 2),
                ("just asking".into(), Some(2), 0),
                ("edit a".into(), Some(1), 1),
                ("plan it".into(), Some(1), 0),
            ]
        );
        assert!(points.iter().all(|p| p.prompt.is_some()));
    }

    #[test]
    fn turns_with_no_message_on_screen_are_code_only_points() {
        let messages = said(&["edit b"]);
        let turns = [
            turn(1, "older edit", 1),
            turn(2, "edit b", 1),
            turn(3, "[subagent report]", 1),
        ];
        let points = rewind_points(&messages, &turns);
        let summary: Vec<(String, bool, Option<u64>)> = points
            .iter()
            .map(|p| (p.excerpt.clone(), p.prompt.is_some(), p.code_turn))
            .collect();
        assert_eq!(
            summary,
            [
                ("[subagent report]".into(), false, Some(3)),
                ("edit b".into(), true, Some(2)),
                ("older edit".into(), false, Some(1)),
            ]
        );
    }

    #[test]
    fn repeated_prompts_count_their_later_repeats() {
        let messages = said(&["run tests", "fix", "run tests"]);
        let points = rewind_points(&messages, &[]);
        let repeats: Vec<(String, usize)> = points
            .iter()
            .map(|p| (p.excerpt.clone(), p.later_repeats))
            .collect();
        assert_eq!(
            repeats,
            [
                ("run tests".into(), 0),
                ("fix".into(), 0),
                ("run tests".into(), 1)
            ]
        );
        assert!(points.iter().all(|p| p.code_turn.is_none()));
    }
}
