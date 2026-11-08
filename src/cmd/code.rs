//! `gz code`: the assistant in the terminal. In an interactive terminal it
//! opens the full coding-CLI interface (`crate::code_ui`); one-shot prompts
//! and piped use get plain streamed lines, with permission prompts inline.

use anyhow::{Context, Result};
use owo_colors::OwoColorize;
use std::io::{BufRead, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use crate::assistant::models::{self, Catalog, Selection};
use crate::assistant::tools::Access;
use crate::assistant::{
    Approver, AssistantEvent, Decision, DecisionFuture, PermissionRequest, Session, TurnEnd,
};
use crate::cli::CodeArgs;
use crate::cmd::Runtime;
use crate::subagents::{render as subagent_render, Board, SubagentEvent};

const HELP: &str =
    "/new  start a new conversation\n/exit quit (or Ctrl+D)\nCtrl+C stops the current answer";

pub async fn run(rt: &Runtime, args: &CodeArgs) -> Result<()> {
    let root = std::env::current_dir().context("cannot read the current folder")?;
    let mut session = Session::new(rt.http.clone(), &root)?;
    session.set_subagents(rt.http.resolved().profile.code_subagents.unwrap_or(true));
    if args.yes_reads {
        session.allow_all_reads();
    }
    // What the interactive UI does first: reprint a resumed conversation,
    // or open the picker.
    let mut start = Vec::new();
    if let Some(id) = &args.conversation {
        session.continue_conversation(id);
        start.push(crate::code_ui::Effect::Resume {
            id: id.clone(),
            title: "this conversation".into(),
        });
    } else if args.continue_last {
        let last = crate::assistant::history::Index::open()
            .ok()
            .and_then(|index| index.last(&root));
        match last {
            Some(entry) => {
                session.continue_conversation(&entry.id);
                start.push(crate::code_ui::Effect::Resume {
                    id: entry.id,
                    title: entry.title,
                });
            }
            None => eprintln!("No earlier gz code conversation in this folder; starting a new one"),
        }
    } else if args.resume {
        start.push(crate::code_ui::Effect::ListConversations);
    }
    let interactive = args.prompt.is_none()
        && std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
        && rt.format == crate::output::OutputFormat::Table;
    // The interactive UI draws at once and loads the model list in the
    // background; an explicit --model/--effort is validated up front.
    let explicit = args.model.is_some() || args.effort.is_some();
    let (catalog, selection) = if interactive && !explicit {
        (None, None)
    } else {
        choose_model(rt, args, interactive).await?
    };
    session.set_model(selection.clone());
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
    if interactive {
        let resolved = rt.http.resolved();
        let identity = crate::code_ui::Identity {
            site: resolved
                .web_url()
                .map(|u| u.trim_start_matches("https://").to_string()),
            user: resolved.profile.username.clone(),
            profile: resolved.profile_name.clone(),
        };
        let models = match catalog {
            Some(catalog) => crate::code_ui::Models::Ready(Box::new(catalog), selection),
            None => crate::code_ui::Models::Loading(spawn_model_load(rt, args)),
        };
        let saved = saved_model_hint(rt);
        return crate::code_ui::run(session, identity, models, saved, start).await;
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

/// The model for this session: `--model`/`--effort` first, then the
/// profile's saved choice; `None` means the server default. The list is
/// loaded only when it is needed (always for the interactive picker).
/// Loads the model list off the startup path; resolves to the catalog and
/// the saved choice, or `None` if the list can't be loaded.
fn spawn_model_load(rt: &Runtime, args: &CodeArgs) -> crate::code_ui::ModelLoad {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let http = rt.http.clone();
    let profile = rt.http.resolved().profile.clone();
    let wanted = args.model.clone().or(profile.code_model);
    let effort = args.effort.clone().or(profile.code_effort);
    tokio::spawn(async move {
        let loaded = models::load(&http).await.ok().map(|catalog| {
            let selection = match &wanted {
                Some(name) => catalog.resolve(name, effort.as_deref()),
                None => effort.as_deref().and_then(|e| {
                    let default = catalog.default_selection()?;
                    catalog.resolve(&default.model.key(), Some(e))
                }),
            };
            (catalog, selection)
        });
        let _ = tx.send(loaded);
    });
    rx
}

/// The saved model as stored (`opus · max`), shown until the list loads.
fn saved_model_hint(rt: &Runtime) -> Option<String> {
    let profile = &rt.http.resolved().profile;
    let model = profile.code_model.clone()?;
    Some(match &profile.code_effort {
        Some(effort) => format!("{model} · {effort}"),
        None => model,
    })
}

async fn choose_model(
    rt: &Runtime,
    args: &CodeArgs,
    interactive: bool,
) -> Result<(Option<Catalog>, Option<Selection>)> {
    let profile = &rt.http.resolved().profile;
    let wanted = args.model.clone().or_else(|| profile.code_model.clone());
    let effort = args.effort.clone().or_else(|| profile.code_effort.clone());
    if !interactive && wanted.is_none() && effort.is_none() {
        return Ok((None, None));
    }
    let catalog = match models::load(&rt.http).await {
        Ok(catalog) => catalog,
        Err(err) if args.model.is_some() => return Err(err),
        Err(_) => return Ok((None, None)),
    };
    let selection = match &wanted {
        Some(name) => match catalog.resolve(name, effort.as_deref()) {
            Some(selection) => Some(selection),
            None if args.model.is_some() => anyhow::bail!(
                "unknown model '{name}'; available: {}",
                catalog
                    .models()
                    .iter()
                    .map(|m| m.key())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            // A saved choice that no longer exists falls back to the default.
            None => None,
        },
        None => effort.as_deref().and_then(|e| {
            let default = catalog.default_selection()?;
            catalog.resolve(&default.model.key(), Some(e))
        }),
    };
    if args.model.is_some() || args.effort.is_some() {
        if let Some(selection) = &selection {
            remember_model(&rt.http.resolved().profile_name, selection)?;
        }
    }
    Ok((Some(catalog), selection))
}

/// Saves the choice on the profile so the next `gz code` starts with it.
/// Saves `/subagents on|off` for this profile (on is the default, so it's
/// stored only when off).
pub fn remember_subagents(profile: &str, enabled: bool) -> Result<()> {
    let mut config = crate::config::load_config()?;
    config.profile_mut(profile).code_subagents = (!enabled).then_some(false);
    crate::config::save_config(&config).map(|_| ())
}

pub fn remember_model(profile: &str, selection: &Selection) -> Result<()> {
    let mut config = crate::config::load_config()?;
    let entry = config.profile_mut(profile);
    entry.code_model = Some(selection.model.key());
    entry.code_effort = selection.effort.clone();
    crate::config::save_config(&config).map(|_| ())
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
    let prepared = session.prepare(prompt, &[])?;
    for note in &prepared.notes {
        eprintln!("{}", format!("  ⎿  {note}").dimmed());
    }
    let canceller = session.canceller();
    let mut subagents = Board::default();
    let mut render = |event: AssistantEvent| render_event(event, mid_line, &mut subagents);
    let end = tokio::select! {
        end = session.send_prepared(&prepared, approver, &mut render) => end?,
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

fn render_event(event: AssistantEvent, mid_line: &AtomicBool, subagents: &mut Board) {
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
        AssistantEvent::Subagent(event) => {
            let now = Instant::now();
            let line = match subagents.apply(&event, now) {
                Some(done) => subagent_render::compact_line(&done, now),
                None => match &event {
                    SubagentEvent::Started { id, .. } => match subagents.get(id) {
                        Some(agent) => subagent_render::compact_line(agent, now),
                        None => return,
                    },
                    _ => return,
                },
            };
            note(mid_line, format!("{}", line.dimmed()));
        }
        // Plain output has no pages: name the subagent on its tool lines.
        AssistantEvent::ForSubagent { id, event } => match *event {
            AssistantEvent::ToolStarted { summary, .. } => note(
                mid_line,
                format!(
                    "{} {}",
                    "⚙".cyan(),
                    format!("[{}] {summary}", subagents.name_of(&id)).dimmed()
                ),
            ),
            other => render_event(other, mid_line, subagents),
        },
        AssistantEvent::Usage(_) | AssistantEvent::Context(_) => {}
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
            if let Some(name) = &request.asked_by {
                eprintln!("  {}", format!("asked by subagent {name}").dimmed());
            }
            if let Some(preview) = &request.preview {
                eprintln!("{}", render_preview(request.access, preview));
            }
            eprint!("{}", repl_choices(request));
            std::io::stderr().flush().ok();
            let answer = tokio::task::spawn_blocking(|| {
                let mut line = String::new();
                std::io::stdin().lock().read_line(&mut line).map(|_| line)
            })
            .await;
            match answer {
                Ok(Ok(line)) => parse_decision(&line, request.always_scope.is_some()),
                _ => Decision::Deny,
            }
        })
    }
}

/// `  [y] once · [c] this chat (…) · [a] always (…) · [n] deny › `; the
/// always choice is left out when the request can't offer it.
fn repl_choices(request: &PermissionRequest) -> String {
    let mut parts = vec![
        format!("{} once", "[y]".cyan()),
        format!("{} this chat ({})", "[c]".cyan(), request.chat_scope),
    ];
    match &request.always_scope {
        Some(scope) => parts.push(format!("{} always ({scope})", "[a]".cyan())),
        None => parts.push(
            "always isn't available for destructive actions"
                .dimmed()
                .to_string(),
        ),
    }
    parts.push(format!("{} deny", "[n]".cyan()));
    format!("  {} › ", parts.join(" · "))
}

/// Anything but an explicit yes is a no; "always" only when offered.
fn parse_decision(answer: &str, always_offered: bool) -> Decision {
    match answer.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" | "once" => Decision::AllowOnce,
        "c" | "chat" => Decision::AllowChat,
        "a" | "always" if always_offered => Decision::AllowAlways,
        _ => Decision::Deny,
    }
}

fn render_preview(access: Access, preview: &str) -> String {
    if access == Access::Script {
        return render_script(preview);
    }
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

/// A script as the user reviews it: dim line numbers, dim comments.
fn render_script(code: &str) -> String {
    code.lines()
        .enumerate()
        .map(|(i, line)| {
            let number = format!("{:>4} │", i + 1).dimmed().to_string();
            let trimmed = line.trim_start();
            let body = if trimmed.starts_with('#') || trimmed.starts_with("//") {
                line.dimmed().to_string()
            } else {
                line.to_string()
            };
            format!("{number} {body}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remembers_the_model_on_the_profile() {
        use crate::assistant::models::{GroundzeroMode, Model};
        let dir = std::env::temp_dir().join(format!("gz-code-model-{}", std::process::id()));
        std::env::set_var("XDG_CONFIG_HOME", &dir);
        let selection = Selection {
            model: Model::Groundzero(GroundzeroMode {
                value: "opus".into(),
                label: "Claude Opus 5".into(),
                efforts: vec!["high".into()],
            }),
            effort: Some("high".into()),
        };
        remember_model("p", &selection).unwrap();
        let config = crate::config::load_config().unwrap();
        std::env::remove_var("XDG_CONFIG_HOME");
        let profile = config.profile("p");
        assert_eq!(profile.code_model.as_deref(), Some("opus"));
        assert_eq!(profile.code_effort.as_deref(), Some("high"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn only_explicit_answers_allow() {
        assert_eq!(parse_decision("y\n", true), Decision::AllowOnce);
        assert_eq!(parse_decision(" c ", true), Decision::AllowChat);
        assert_eq!(parse_decision(" A ", true), Decision::AllowAlways);
        assert_eq!(
            parse_decision("a", false),
            Decision::Deny,
            "always not offered"
        );
        assert_eq!(parse_decision("", true), Decision::Deny);
        assert_eq!(parse_decision("sure", true), Decision::Deny);
    }

    #[test]
    fn colors_diff_lines() {
        let text = render_preview(Access::Write, "--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old\n+new");
        assert!(text.contains("old") && text.contains("new"));
        assert!(render_preview(Access::Command, "ls").contains("ls"));
        let script = render_preview(Access::Script, "import gz\n# note\nprint(1)");
        assert!(script.contains("   3 │") && script.contains("print(1)"));
    }
}
