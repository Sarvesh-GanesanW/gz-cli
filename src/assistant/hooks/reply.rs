//! What a hook answered: its exit code, or a JSON object on stdout.
//!
//! Exit 0 is fine (plain stdout is shown, or added as context where the
//! event takes it), exit 2 blocks (stderr says why), anything else is a
//! problem the user sees and gz carries on. A hook that exits 0 may print
//! one JSON object instead:
//!
//! ```json
//! {"decision": "allow|deny|ask|block", "reason": "…",
//!  "additionalContext": "…", "continue": false, "stopReason": "…",
//!  "suppressOutput": true}
//! ```

use serde::Deserialize;

use super::runner::{Ending, Ran};

/// The exit code that blocks.
pub const EXIT_BLOCK: i32 = 2;

/// What a hook decided about the thing it was asked about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny,
    Ask,
    /// Exit 2, or `"decision": "block"`.
    Block,
}

impl Decision {
    fn parse(word: &str) -> Option<Self> {
        match word.trim().to_ascii_lowercase().as_str() {
            "allow" | "approve" => Some(Decision::Allow),
            "deny" => Some(Decision::Deny),
            "ask" => Some(Decision::Ask),
            "block" => Some(Decision::Block),
            _ => None,
        }
    }

    /// Deny and block both stop the thing from happening.
    pub fn is_refusal(self) -> bool {
        matches!(self, Decision::Deny | Decision::Block)
    }
}

/// One hook's answer, read from how it ended and what it printed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reply {
    pub decision: Option<Decision>,
    /// Why it decided (exit 2: its stderr, else stdout).
    pub reason: Option<String>,
    /// Text for the model: `additionalContext`, or plain stdout on exit 0.
    pub context: Option<String>,
    /// `"continue": false`: stop the turn, with this reason.
    pub halt: Option<String>,
    /// Don't show its stdout to the user.
    pub suppress_output: bool,
    /// What went wrong (other exits, a timeout, a bad JSON answer).
    pub problem: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct JsonReply {
    decision: Option<String>,
    reason: Option<String>,
    additional_context: Option<String>,
    #[serde(rename = "continue")]
    keep_going: Option<bool>,
    stop_reason: Option<String>,
    #[serde(default)]
    suppress_output: bool,
}

/// Reads `ran` as a reply.
pub fn read(ran: &Ran) -> Reply {
    match &ran.ending {
        Ending::Exited(0) => read_success(ran.stdout.trim()),
        Ending::Exited(EXIT_BLOCK) => Reply {
            decision: Some(Decision::Block),
            reason: Some(said(ran).unwrap_or_else(|| "no reason given".to_string())),
            ..Reply::default()
        },
        Ending::Exited(code) => problem(format!("exited with {code}")),
        Ending::Signalled => problem("was killed by a signal".to_string()),
        Ending::TimedOut(limit) => problem(format!(
            "ran past its {}s timeout and was stopped",
            limit.as_secs()
        )),
        Ending::Failed(why) => problem(why.clone()),
    }
}

fn problem(why: String) -> Reply {
    Reply {
        problem: Some(why),
        ..Reply::default()
    }
}

fn read_success(stdout: &str) -> Reply {
    let plain = || Reply {
        context: (!stdout.is_empty()).then(|| stdout.to_string()),
        ..Reply::default()
    };
    if !stdout.starts_with('{') {
        return plain();
    }
    let Ok(json) = serde_json::from_str::<JsonReply>(stdout) else {
        return plain();
    };
    let decision = match json.decision.as_deref().map(|w| (w, Decision::parse(w))) {
        None => None,
        Some((_, Some(decision))) => Some(decision),
        Some((word, None)) => {
            return problem(format!(
                "answered an unknown decision '{word}' (use allow, deny, ask or block)"
            ))
        }
    };
    let halt = (json.keep_going == Some(false)).then(|| {
        json.stop_reason
            .clone()
            .filter(|r| !r.trim().is_empty())
            .unwrap_or_else(|| "a hook stopped the turn".to_string())
    });
    Reply {
        decision,
        reason: json.reason.filter(|r| !r.trim().is_empty()),
        context: json.additional_context.filter(|c| !c.trim().is_empty()),
        halt,
        suppress_output: json.suppress_output,
        problem: None,
    }
}

/// What a hook said for the model: stderr, else stdout.
pub fn said(ran: &Ran) -> Option<String> {
    [&ran.stderr, &ran.stdout]
        .into_iter()
        .map(|text| text.trim())
        .find(|text| !text.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ran(code: i32, stdout: &str, stderr: &str) -> Ran {
        Ran {
            ending: Ending::Exited(code),
            stdout: stdout.into(),
            stderr: stderr.into(),
        }
    }

    #[test]
    fn exit_codes_mean_ok_block_and_problem() {
        let ok = read(&ran(0, "note\n", ""));
        assert_eq!(ok.context.as_deref(), Some("note"));
        assert_eq!(ok.decision, None);

        let blocked = read(&ran(2, "", "not here\n"));
        assert_eq!(blocked.decision, Some(Decision::Block));
        assert_eq!(blocked.reason.as_deref(), Some("not here"));

        let failed = read(&ran(1, "", "oops"));
        assert_eq!(failed.problem.as_deref(), Some("exited with 1"));
    }

    #[test]
    fn a_json_answer_carries_decision_context_and_halt() {
        let reply = read(&ran(
            0,
            r#"{"decision":"deny","reason":"no","additionalContext":"see docs","continue":false,"stopReason":"done","suppressOutput":true}"#,
            "",
        ));
        assert_eq!(reply.decision, Some(Decision::Deny));
        assert_eq!(reply.reason.as_deref(), Some("no"));
        assert_eq!(reply.context.as_deref(), Some("see docs"));
        assert_eq!(reply.halt.as_deref(), Some("done"));
        assert!(reply.suppress_output);

        let halted = read(&ran(0, r#"{"continue": false}"#, ""));
        assert_eq!(halted.halt.as_deref(), Some("a hook stopped the turn"));
    }

    #[test]
    fn an_unknown_decision_is_a_problem_and_other_json_is_plain_text() {
        let bad = read(&ran(0, r#"{"decision":"maybe"}"#, ""));
        assert!(bad.problem.unwrap().contains("unknown decision 'maybe'"));

        let text = read(&ran(0, "{not json", ""));
        assert_eq!(text.context.as_deref(), Some("{not json"));
    }
}
