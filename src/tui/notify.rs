//! Short notices in the status bar ("Run 42 finished", "Stream stopped")
//! that any view can raise. Each shows for a few seconds; `ctrl+g` jumps to
//! where it came from.

use std::collections::VecDeque;
use std::fmt;
use std::rc::Rc;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::tui::nav::ResourceId;
use crate::tui::screen::Goto;
use crate::tui::theme::BadgeTone;
use crate::tui::views::View;

/// How long one notice stays in the status bar.
pub const SHOW_FOR: Duration = Duration::from_secs(6);
/// Notices kept waiting at most; older ones are dropped.
const QUEUE_LIMIT: usize = 20;

/// Where a notice leads.
#[derive(Clone)]
#[allow(
    dead_code,
    reason = "spec vocabulary for the screens being converted next"
)]
pub enum Jump {
    Resource(ResourceId),
    /// Builds the view to open (a run viewer for that run, …).
    View(Rc<dyn Fn() -> Box<dyn View>>),
    /// Follows a link from this row (selects a failed run in its list).
    Follow(Goto, Value),
}

impl fmt::Debug for Jump {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Jump::Resource(id) => f.debug_tuple("Resource").field(id).finish(),
            Jump::View(_) => f.write_str("View"),
            Jump::Follow(goto, _) => f.debug_tuple("Follow").field(&goto.target).finish(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Notification {
    pub text: String,
    pub tone: BadgeTone,
    pub jump: Option<Jump>,
}

#[allow(
    dead_code,
    reason = "spec vocabulary for the screens being converted next"
)]
impl Notification {
    pub fn new(tone: BadgeTone, text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            tone,
            jump: None,
        }
    }

    pub fn jump_to(self, jump: Jump) -> Self {
        Self {
            jump: Some(jump),
            ..self
        }
    }
}

#[derive(Debug, Default)]
pub struct Notifier {
    queue: VecDeque<Notification>,
    shown_since: Option<Instant>,
}

impl Notifier {
    pub fn push(&mut self, notification: Notification) {
        if self.queue.len() >= QUEUE_LIMIT {
            self.queue.pop_front();
        }
        self.queue.push_back(notification);
    }

    /// The notice to show at `now`, moving on once one has had its time.
    pub fn current(&mut self, now: Instant) -> Option<&Notification> {
        loop {
            self.queue.front()?;
            match self.shown_since {
                None => {
                    self.shown_since = Some(now);
                    break;
                }
                Some(since) if now.duration_since(since) >= SHOW_FOR => {
                    self.queue.pop_front();
                    self.shown_since = None;
                }
                Some(_) => break,
            }
        }
        self.queue.front()
    }

    /// The notice on screen now, without advancing (for drawing).
    pub fn showing(&self) -> Option<&Notification> {
        self.shown_since.and(self.queue.front())
    }

    /// Removes the notice on screen and returns where it leads.
    pub fn take_jump(&mut self) -> Option<Jump> {
        self.shown_since?;
        self.shown_since = None;
        self.queue.pop_front().and_then(|n| n.jump)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shows_each_notice_for_its_time_then_the_next() {
        let start = Instant::now();
        let mut notifier = Notifier::default();
        notifier.push(Notification::new(BadgeTone::Good, "first"));
        notifier.push(Notification::new(BadgeTone::Bad, "second"));
        assert_eq!(notifier.current(start).unwrap().text, "first");
        assert_eq!(
            notifier.current(start + SHOW_FOR / 2).unwrap().text,
            "first"
        );
        assert_eq!(notifier.current(start + SHOW_FOR).unwrap().text, "second");
        assert!(notifier.current(start + SHOW_FOR * 3).is_none());
    }

    #[test]
    fn jumping_consumes_the_notice_on_screen() {
        let start = Instant::now();
        let target = ResourceId {
            module: 1,
            resource: 0,
        };
        let mut notifier = Notifier::default();
        notifier.push(Notification::new(BadgeTone::Info, "ready").jump_to(Jump::Resource(target)));
        assert!(notifier.take_jump().is_none(), "nothing is on screen yet");
        notifier.current(start);
        assert!(matches!(notifier.take_jump(), Some(Jump::Resource(id)) if id == target));
        assert!(notifier.showing().is_none());
    }
}
