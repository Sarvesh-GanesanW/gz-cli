//! `gz code`: the assistant as a terminal REPL, like a coding CLI. Answers
//! stream in, local tool calls show as compact lines, and every permission
//! prompt appears inline with the diff or command it would run.

use anyhow::{Context, Result};
use owo_colors::OwoColorize;
use std::io::{BufRead, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::assistant::tools::Access;
use crate::assistant::{
    Approver, AssistantEvent, Decision, DecisionFuture, PermissionRequest, Session, TurnEnd,
};
use crate::cli::CodeArgs;
use crate::cmd::Runtime;

const HELP: &str =
    "/new  start a new conversation\n/exit quit (or Ctrl+D)\nCtrl+C stops the current answer";

pub async fn run(rt: &Runtime, args: &CodeArgs) -> Result<()> {
    let root = std::env::current_dir().context("cannot read the current folder")?;
    let mut session = Session::new(rt.http.clone(), &root)?;
    if args.yes_reads {
        session.allow_all_reads();
    }
    if let Some(id) = &args.conversation {
        session.continue_conversation(id);
    }
    let mid_line = Arc::new(AtomicBool::new(false));
    let approver = TerminalApprover {
        mid_line: mid_line.clone(),
    };
    if let Some(prompt) = &args.prompt {
        let prompt = crate::cmd::read_text_arg(prompt)?;
        return match turn(&mut session, &prompt, &approver, &mid_line).await? {
            TurnEnd::Failed(why) => anyhow::bail!("{why}"),
            _ => Ok(()),
        };
    }
    print_banner(&session);
    while let Some(line) = read_prompt().await? {
        match line.trim() {
            "" => continue,
            "/exit" | "/quit" => break,
            "/help" => eprintln!("{}", HELP.dimmed()),
            "/new" => {
                session.new_conversation();
                eprintln!("{}", "· new conversation".dimmed());
            }
            prompt => {
                if let TurnEnd::Failed(why) =
                    turn(&mut session, prompt, &approver, &mid_line).await?
                {
                    crate::output::warn(&why);
                }
            }
        }
    }
    eprintln!(
        "{}",
        format!("continue later: gz code -c {}", session.conversation_id()).dimmed()
    );
    Ok(())
}

fn print_banner(session: &Session) {
    eprintln!(
        "{} {}  {}",
        "◆ gz code".cyan().bold(),
        session.workspace_name().bold(),
        session.workspace().display().to_string().dimmed()
    );
    eprintln!(
        "{}",
        "Ask anything about this folder. gz asks before reading, editing, or running anything. /help for commands."
            .dimmed()
    );
}

/// Reads one prompt line; `None` on Ctrl+D, end of input, or Ctrl+C.
async fn read_prompt() -> Result<Option<String>> {
    eprint!("\n{} ", "›".cyan().bold());
    std::io::stderr().flush().ok();
    let read = tokio::task::spawn_blocking(|| {
        let mut line = String::new();
        let n = std::io::stdin().lock().read_line(&mut line)?;
        Ok::<_, std::io::Error>((n > 0).then_some(line))
    });
    tokio::select! {
        line = read => Ok(line.context("input reader stopped")?.context("cannot read input")?),
        _ = tokio::signal::ctrl_c() => {
            eprintln!();
            Ok(None)
        }
    }
}

/// Runs one turn; Ctrl+C cancels it on the server and returns to the prompt.
async fn turn(
    session: &mut Session,
    prompt: &str,
    approver: &TerminalApprover,
    mid_line: &Arc<AtomicBool>,
) -> Result<TurnEnd> {
    let canceller = session.canceller();
    let mut render = |event: AssistantEvent| render_event(event, mid_line);
    let end = tokio::select! {
        end = session.send(prompt, approver, &mut render) => end?,
        _ = tokio::signal::ctrl_c() => {
            canceller.cancel().await.ok();
            TurnEnd::Cancelled
        }
    };
    end_line(mid_line);
    if end == TurnEnd::Cancelled {
        eprintln!("{}", "· stopped".dimmed());
    }
    Ok(end)
}

fn end_line(mid_line: &AtomicBool) {
    if mid_line.swap(false, Ordering::SeqCst) {
        println!();
    }
}

fn note(mid_line: &AtomicBool, text: String) {
    end_line(mid_line);
    eprintln!("{text}");
}

fn render_event(event: AssistantEvent, mid_line: &AtomicBool) {
    match event {
        AssistantEvent::Text(text) => {
            print!("{text}");
            std::io::stdout().flush().ok();
            mid_line.store(!text.ends_with('\n'), Ordering::SeqCst);
        }
        AssistantEvent::ToolStarted { summary, .. } => {
            note(mid_line, format!("{} {}", "⚙".cyan(), summary.dimmed()))
        }
        AssistantEvent::ToolFinished {
            is_error, detail, ..
        } => {
            let mark = if is_error {
                "✗".red().to_string()
            } else {
                "✓".green().to_string()
            };
            note(mid_line, format!("  {mark} {}", detail.dimmed()));
        }
        AssistantEvent::Notice(text) => note(mid_line, format!("{}", format!("· {text}").dimmed())),
        AssistantEvent::Done(_) => end_line(mid_line),
    }
}

/// Asks on the terminal: `allow once [y] / this session [a] / deny [n]`.
/// Without a terminal there is nobody to ask, so the answer is deny.
struct TerminalApprover {
    mid_line: Arc<AtomicBool>,
}

impl Approver for TerminalApprover {
    fn decide<'a>(&'a self, request: &'a PermissionRequest) -> DecisionFuture<'a> {
        Box::pin(async move {
            end_line(&self.mid_line);
            if !std::io::stdin().is_terminal() {
                eprintln!(
                    "{} {} {}",
                    "✗".red(),
                    request.summary,
                    "(denied: no terminal to ask; use --yes-reads for reads)".dimmed()
                );
                return Decision::Deny;
            }
            eprintln!("{} {}", "?".yellow().bold(), request.summary.bold());
            if let Some(preview) = &request.preview {
                eprintln!("{}", render_preview(request.access, preview));
            }
            eprint!(
                "  allow once {} · {} {} · deny {} › ",
                "[y]".cyan(),
                request.session_scope,
                "[a]".cyan(),
                "[n]".cyan()
            );
            std::io::stderr().flush().ok();
            let answer = tokio::task::spawn_blocking(|| {
                let mut line = String::new();
                std::io::stdin().lock().read_line(&mut line).map(|_| line)
            })
            .await;
            match answer {
                Ok(Ok(line)) => parse_decision(&line),
                _ => Decision::Deny,
            }
        })
    }
}

/// Anything but an explicit yes is a no.
fn parse_decision(answer: &str) -> Decision {
    match answer.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Decision::AllowOnce,
        "a" | "always" => Decision::AllowSession,
        _ => Decision::Deny,
    }
}

fn render_preview(access: Access, preview: &str) -> String {
    if access == Access::Command {
        return format!("  {} {}", "$".dimmed(), preview.bold());
    }
    if access != Access::Write {
        return preview.dimmed().to_string();
    }
    preview
        .lines()
        .map(|line| {
            if line.starts_with("+++") || line.starts_with("---") {
                line.bold().to_string()
            } else if line.starts_with('+') {
                line.green().to_string()
            } else if line.starts_with('-') {
                line.red().to_string()
            } else if line.starts_with("@@") {
                line.cyan().to_string()
            } else {
                line.dimmed().to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_explicit_answers_allow() {
        assert_eq!(parse_decision("y\n"), Decision::AllowOnce);
        assert_eq!(parse_decision(" A "), Decision::AllowSession);
        assert_eq!(parse_decision(""), Decision::Deny);
        assert_eq!(parse_decision("sure"), Decision::Deny);
    }

    #[test]
    fn colors_diff_lines() {
        let text = render_preview(Access::Write, "--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old\n+new");
        assert!(text.contains("old") && text.contains("new"));
        assert!(render_preview(Access::Command, "ls").contains("ls"));
    }
}
