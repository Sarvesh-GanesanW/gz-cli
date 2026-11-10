//! What the model asks the user directly (the `ask_user_question` and
//! `exit_plan_mode` tools): multiple-choice questions, and a plan
//! to approve before gz leaves plan mode. Front ends show a [`Query`] and
//! send back a [`Reply`]; the session turns the reply into the tool result.

use anyhow::{bail, Context, Result};
use serde_json::Value;

/// One multiple-choice question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    pub question: String,
    /// A short tag shown above it, like "Auth method".
    pub header: String,
    pub options: Vec<Choice>,
    pub multi_select: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub label: String,
    pub description: Option<String>,
}

/// Something the model asks the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Query {
    Questions(Vec<Question>),
    /// A plan to approve before leaving plan mode.
    Plan(String),
}

/// How the user wants gz to work once a plan is approved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkMode {
    Auto,
    AcceptEdits,
    Ask,
}

/// The user's reply to a [`Query`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// One entry per question: the chosen labels or typed text, `None` if skipped.
    Answers(Vec<Option<String>>),
    PlanApproved(WorkMode),
    /// Keep planning, with what to change.
    KeepPlanning(Option<String>),
    /// Nobody could answer (no terminal, the UI closed, the run ended).
    NoAnswer,
}

/// Bounds the chat server also checks.
const MAX_QUESTIONS: usize = 4;
const MIN_OPTIONS: usize = 2;
const MAX_OPTIONS: usize = 4;

/// The questions of an `ask_user_question` call.
pub fn parse_questions(input: &Value) -> Result<Vec<Question>> {
    let items = input["questions"]
        .as_array()
        .context("missing list field 'questions'")?;
    if items.is_empty() || items.len() > MAX_QUESTIONS {
        bail!("ask 1 to {MAX_QUESTIONS} questions at a time");
    }
    items.iter().map(parse_question).collect()
}

fn parse_question(item: &Value) -> Result<Question> {
    let text = |key: &str| item[key].as_str().map(str::trim).filter(|t| !t.is_empty());
    let options = item["options"]
        .as_array()
        .context("each question needs a list of options")?
        .iter()
        .map(|option| {
            let label = option["label"]
                .as_str()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .context("each option needs a label")?;
            Ok(Choice {
                label: label.to_string(),
                description: option["description"]
                    .as_str()
                    .map(str::trim)
                    .filter(|d| !d.is_empty())
                    .map(str::to_string),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    if !(MIN_OPTIONS..=MAX_OPTIONS).contains(&options.len()) {
        bail!("each question needs {MIN_OPTIONS} to {MAX_OPTIONS} options");
    }
    Ok(Question {
        question: text("question")
            .context("each question needs its text")?
            .to_string(),
        header: text("header").unwrap_or_default().to_string(),
        options,
        multi_select: item["multi_select"].as_bool().unwrap_or(false),
    })
}

/// The tool result for an `ask_user_question` reply.
pub fn answers_text(questions: &[Question], reply: &Reply) -> String {
    let Reply::Answers(answers) = reply else {
        return "The user didn't answer; continue with your best judgement and say what you assumed."
            .to_string();
    };
    questions
        .iter()
        .zip(answers.iter().chain(std::iter::repeat(&None)))
        .map(|(question, answer)| match answer {
            Some(answer) => format!("{} → {answer}", question.question),
            None => format!("{} → (skipped)", question.question),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The tool result for an `exit_plan_mode` reply.
pub fn plan_text(reply: &Reply) -> String {
    match reply {
        Reply::PlanApproved(mode) => {
            let how = match mode {
                WorkMode::Auto => "auto mode (gz reviews each action)",
                WorkMode::AcceptEdits => "edits accepted without asking",
                WorkMode::Ask => "gz asking before each edit and command",
            };
            format!("The user approved the plan. Plan mode is off; carry it out now, with {how}.")
        }
        Reply::KeepPlanning(Some(note)) if !note.trim().is_empty() => format!(
            "The user wants changes to the plan before you start: {note}\nStay in plan mode: \
             revise the plan and call exit_plan_mode again."
        ),
        Reply::KeepPlanning(_) => "The user wants to keep planning. Stay in plan mode, ask what \
                                   to change if it is unclear, and call exit_plan_mode again."
            .to_string(),
        Reply::Answers(_) | Reply::NoAnswer => {
            "Nobody approved the plan, so plan mode stays on. Wait for the user.".to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn input() -> Value {
        json!({"questions": [
            {"question": "Which database?", "header": "Database", "options": [
                {"label": "Postgres (Recommended)", "description": "already in use"},
                {"label": "SQLite"}]},
            {"question": "Which checks?", "header": "Checks", "multi_select": true, "options": [
                {"label": "Lint"}, {"label": "Tests"}]}
        ]})
    }

    #[test]
    fn questions_parse_with_their_options() {
        let questions = parse_questions(&input()).unwrap();
        assert_eq!(questions.len(), 2);
        assert_eq!(
            questions[0].options[0].description.as_deref(),
            Some("already in use")
        );
        assert!(questions[1].multi_select);
    }

    #[test]
    fn bad_questions_say_what_is_wrong() {
        let err = |v: Value| format!("{:#}", parse_questions(&v).unwrap_err());
        assert!(err(json!({"questions": []})).contains("1 to 4"));
        assert!(
            err(json!({"questions": [{"question": "q", "options": [{"label": "a"}]}]}))
                .contains("2 to 4 options")
        );
    }

    #[test]
    fn answers_and_plans_become_tool_results() {
        let questions = parse_questions(&input()).unwrap();
        let reply = Reply::Answers(vec![Some("SQLite".into()), None]);
        assert_eq!(
            answers_text(&questions, &reply),
            "Which database? → SQLite\nWhich checks? → (skipped)"
        );
        assert!(answers_text(&questions, &Reply::NoAnswer).contains("didn't answer"));
        assert!(plan_text(&Reply::PlanApproved(WorkMode::Auto)).contains("auto mode"));
        assert!(plan_text(&Reply::KeepPlanning(Some("use SQLite".into()))).contains("use SQLite"));
    }
}
