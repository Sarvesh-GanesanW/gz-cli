//! Getting your attention when gz code finishes a turn or needs you (an
//! approval or a question) while you look elsewhere.
//!
//! When the terminal reports focus, gz notifies only while it is not
//! focused. Otherwise a notification goes out once the moment has waited
//! [`IDLE_THRESHOLD`] without a key press, and a finished turn is not
//! announced at all when a key was pressed within that long before it
//! ended: you were there (or interrupted it yourself). Interactive sessions only: the
//! one-shot and headless modes never get here.

use std::io::Write;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::notify::{self, Notice};

/// Without focus reports, how long a finished turn or an open prompt waits
/// for a key before gz notifies.
pub const IDLE_THRESHOLD: Duration = Duration::from_secs(6);
const TITLE: &str = "gz code";

/// How gz code notifies (`/notifications`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NotifyChoice {
    /// A terminal notification where the terminal shows one, else the bell.
    #[default]
    Auto,
    /// The terminal bell.
    Bell,
    /// A terminal notification escape (OSC 9 or OSC 777).
    Osc,
    /// A desktop notification (macOS, Linux, Windows).
    Desktop,
    Off,
}

impl NotifyChoice {
    pub const ALL: [NotifyChoice; 5] = [
        NotifyChoice::Auto,
        NotifyChoice::Bell,
        NotifyChoice::Osc,
        NotifyChoice::Desktop,
        NotifyChoice::Off,
    ];

    pub fn name(self) -> &'static str {
        match self {
            NotifyChoice::Auto => "auto",
            NotifyChoice::Bell => "bell",
            NotifyChoice::Osc => "osc",
            NotifyChoice::Desktop => "desktop",
            NotifyChoice::Off => "off",
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            NotifyChoice::Auto => "a terminal notification where supported, else the bell",
            NotifyChoice::Bell => "the terminal bell",
            NotifyChoice::Osc => "a terminal notification (OSC 9 / OSC 777)",
            NotifyChoice::Desktop => "a desktop notification",
            NotifyChoice::Off => "never",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|choice| choice.name().eq_ignore_ascii_case(text.trim()))
    }
}

/// What gz is waiting on, as seen once per frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Moment {
    /// A turn is running.
    pub busy: bool,
    /// What gz waits for you to answer (a permission prompt, a question),
    /// in a few words.
    pub needs_you: Option<String>,
}

/// A notification to send now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alert {
    pub message: String,
}

/// Tracks focus, key presses and what gz waits on; says when to notify.
#[derive(Debug)]
pub struct Attention {
    choice: NotifyChoice,
    /// `None` until the terminal reports focus at all.
    focused: Option<bool>,
    last: Moment,
    /// An alert waiting for the idle threshold (no focus reports).
    due: Option<(Instant, Alert)>,
    /// The last key press or paste.
    last_key: Option<Instant>,
}

impl Attention {
    pub fn new(choice: NotifyChoice) -> Self {
        Self {
            choice,
            focused: None,
            last: Moment {
                busy: false,
                needs_you: None,
            },
            due: None,
            last_key: None,
        }
    }

    pub fn choice(&self) -> NotifyChoice {
        self.choice
    }

    pub fn set_choice(&mut self, choice: NotifyChoice) {
        self.choice = choice;
        if choice == NotifyChoice::Off {
            self.due = None;
        }
    }

    /// The terminal reported focus coming or going.
    pub fn focus(&mut self, focused: bool) {
        self.focused = Some(focused);
        if focused {
            self.due = None;
        }
    }

    /// A key press: you are looking, so nothing pending goes out.
    pub fn key_pressed(&mut self, now: Instant) {
        self.due = None;
        self.last_key = Some(now);
    }

    fn key_pressed_within_threshold(&self, now: Instant) -> bool {
        self.last_key
            .is_some_and(|at| now.saturating_duration_since(at) < IDLE_THRESHOLD)
    }

    /// Compares `now_moment` with the last frame's; returns an alert to
    /// send at once, if any.
    pub fn observe(&mut self, moment: Moment, now: Instant) -> Option<Alert> {
        let alert = self.alert_for(&moment);
        let finished = alert.is_some() && moment.needs_you.is_none();
        self.last = moment;
        if self.choice == NotifyChoice::Off {
            return None;
        }
        if let Some(alert) = alert {
            match self.focused {
                Some(true) => self.due = None,
                Some(false) => return Some(alert),
                None if finished && self.key_pressed_within_threshold(now) => {}
                None => self.due = Some((now + IDLE_THRESHOLD, alert)),
            }
        }
        // A prompt answered (or a new turn started) needs no reminder.
        if self.last.busy && self.last.needs_you.is_none() {
            self.due = None;
        }
        match &self.due {
            Some((at, _)) if now >= *at => self.due.take().map(|(_, alert)| alert),
            _ => None,
        }
    }

    fn alert_for(&self, moment: &Moment) -> Option<Alert> {
        if let Some(what) = &moment.needs_you {
            if self.last.needs_you.as_ref() != Some(what) {
                return Some(Alert {
                    message: format!("Needs you: {what}"),
                });
            }
            return None;
        }
        (self.last.busy && !moment.busy).then(|| Alert {
            message: "Finished; ready for your next message".into(),
        })
    }
}

/// How an alert reaches you on this terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    Bell,
    Osc(notify::TerminalNotice),
    Desktop,
}

/// The channel for `choice` on the current terminal (`None`: off).
pub fn channel(choice: NotifyChoice) -> Option<Channel> {
    match choice {
        NotifyChoice::Off => None,
        NotifyChoice::Bell => Some(Channel::Bell),
        NotifyChoice::Desktop => Some(Channel::Desktop),
        NotifyChoice::Osc => Some(Channel::Osc(
            notify::current_terminal_notice().unwrap_or(notify::TerminalNotice::Osc9),
        )),
        NotifyChoice::Auto => Some(
            notify::current_terminal_notice()
                .map(Channel::Osc)
                .unwrap_or(Channel::Bell),
        ),
    }
}

/// Sends `alert`. Terminal alerts are written to `out` at once; a desktop
/// one is handed back for the caller to show in the background.
pub fn send(choice: NotifyChoice, alert: Alert, out: &mut impl Write) -> Option<Notice> {
    let notice = Notice {
        title: TITLE,
        message: alert.message,
    };
    match channel(choice)? {
        Channel::Bell => {
            write_signal(out, "\x07");
            None
        }
        Channel::Osc(kind) => {
            write_signal(out, &notify::terminal_escape_for(kind, &notice));
            None
        }
        Channel::Desktop => Some(notice),
    }
}

fn write_signal(out: &mut impl Write, signal: &str) {
    // A bell that can't be written has nowhere else to go.
    let _ = out.write_all(signal.as_bytes());
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idle() -> Moment {
        Moment {
            busy: false,
            needs_you: None,
        }
    }

    fn busy() -> Moment {
        Moment {
            busy: true,
            needs_you: None,
        }
    }

    fn asking(what: &str) -> Moment {
        Moment {
            busy: true,
            needs_you: Some(what.into()),
        }
    }

    #[test]
    fn unfocused_terminals_hear_at_once_and_focused_ones_never() {
        let now = Instant::now();
        let mut attention = Attention::new(NotifyChoice::Auto);
        attention.focus(false);
        assert_eq!(attention.observe(busy(), now), None);
        let alert = attention.observe(idle(), now).unwrap();
        assert!(alert.message.starts_with("Finished"));
        attention.observe(busy(), now);
        attention.focus(true);
        assert_eq!(attention.observe(idle(), now), None);
        assert_eq!(attention.observe(idle(), now + IDLE_THRESHOLD * 2), None);
    }

    #[test]
    fn without_focus_reports_an_unanswered_prompt_notifies_after_the_threshold() {
        let now = Instant::now();
        let mut attention = Attention::new(NotifyChoice::Bell);
        attention.observe(busy(), now);
        assert_eq!(attention.observe(asking("run cargo test"), now), None);
        let early = now + IDLE_THRESHOLD / 2;
        assert_eq!(attention.observe(asking("run cargo test"), early), None);
        let alert = attention
            .observe(asking("run cargo test"), now + IDLE_THRESHOLD)
            .unwrap();
        assert_eq!(alert.message, "Needs you: run cargo test");
        let later = now + IDLE_THRESHOLD * 3;
        assert_eq!(
            attention.observe(asking("run cargo test"), later),
            None,
            "once"
        );
    }

    #[test]
    fn a_key_or_an_answer_cancels_a_pending_notification() {
        let now = Instant::now();
        let mut attention = Attention::new(NotifyChoice::Bell);
        attention.observe(busy(), now);
        attention.observe(idle(), now);
        attention.key_pressed(now);
        assert_eq!(attention.observe(idle(), now + IDLE_THRESHOLD), None);

        attention.observe(asking("edit a.rs"), now);
        attention.observe(busy(), now);
        assert_eq!(attention.observe(busy(), now + IDLE_THRESHOLD), None);
    }

    #[test]
    fn without_focus_reports_a_turn_you_just_typed_or_interrupted_is_not_announced() {
        let start = Instant::now();
        let mut attention = Attention::new(NotifyChoice::Bell);
        // Enter sends the message; the turn ends two seconds later.
        attention.key_pressed(start);
        attention.observe(busy(), start);
        let end = start + Duration::from_secs(2);
        assert_eq!(attention.observe(idle(), end), None);
        assert_eq!(attention.observe(idle(), end + IDLE_THRESHOLD * 2), None);

        // Esc interrupts a long turn: the cancel ends it at once.
        let long = end + Duration::from_secs(60);
        attention.observe(busy(), long);
        let esc = long + Duration::from_secs(30);
        attention.key_pressed(esc);
        attention.observe(idle(), esc);
        assert_eq!(attention.observe(idle(), esc + IDLE_THRESHOLD * 2), None);
    }

    #[test]
    fn without_focus_reports_a_long_turn_you_left_is_announced_after_the_threshold() {
        let start = Instant::now();
        let mut attention = Attention::new(NotifyChoice::Bell);
        attention.key_pressed(start);
        attention.observe(busy(), start);
        let end = start + IDLE_THRESHOLD * 3;
        assert_eq!(attention.observe(idle(), end), None);
        let alert = attention.observe(idle(), end + IDLE_THRESHOLD).unwrap();
        assert!(alert.message.starts_with("Finished"));
    }

    #[test]
    fn off_never_notifies() {
        let now = Instant::now();
        let mut attention = Attention::new(NotifyChoice::Off);
        attention.focus(false);
        attention.observe(busy(), now);
        assert_eq!(attention.observe(idle(), now), None);
        assert_eq!(channel(NotifyChoice::Off), None);
    }

    #[test]
    fn terminal_channels_write_their_signal() {
        let mut out = Vec::new();
        let alert = Alert {
            message: "Finished".into(),
        };
        assert!(send(NotifyChoice::Bell, alert.clone(), &mut out).is_none());
        assert_eq!(out, b"\x07");
        let mut out = Vec::new();
        send(NotifyChoice::Osc, alert, &mut out);
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("\x1b]"), "{text:?}");
        assert!(
            text.contains("gz code") && text.contains("Finished"),
            "{text:?}"
        );
    }

    #[test]
    fn choices_parse_by_name() {
        for choice in NotifyChoice::ALL {
            assert_eq!(NotifyChoice::parse(choice.name()), Some(choice));
        }
        assert_eq!(NotifyChoice::parse("loud"), None);
    }
}
