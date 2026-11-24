//! `gz code`: the assistant in the terminal. In an interactive terminal it
//! opens the full coding-CLI interface (`crate::code_ui`); one-shot prompts
//! and piped use get plain streamed lines, with permission prompts inline.
//! Scripted runs (`-p`, `--output-format`, `-o json|yaml`, limits) never
//! wait for input and print their result as asked (`crate::cmd::code_headless`).
//! Their prompt may also come piped in on standard input.

use anyhow::{Context, Result};
use owo_colors::OwoColorize;
use std::io::{BufRead, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use crate::assistant::compaction;
use crate::assistant::models::{self, Catalog, Selection};
use crate::assistant::subagent_settings::SubagentSettings;
use crate::assistant::tools::Access;
use crate::assistant::{
    Approver, AssistantEvent, Decision, DecisionFuture, PermissionRequest, Session, TurnEnd,
};
use crate::cli::CodeArgs;
use crate::cmd::code_unattended::{AutoApprover, DecisionLog, Nobody, Recording};
use crate::cmd::stream::{TokenCounts, Usage};
use crate::cmd::Runtime;
use crate::cmd::{code_headless, code_worktree};
use crate::code_ui::usage::Tally;
use crate::output::OutputFormat;
use crate::spend::caps::CapReached;
use crate::spend::ledger::Ledger;
use crate::spend::meter::Meter;
use crate::spend::rates;
use crate::subagents::{render as subagent_render, Board, SubagentEvent};

/// How long an interrupted turn gets to tell the chat before gz moves on.
const INTERRUPT_CLEANUP: std::time::Duration = std::time::Duration::from_secs(10);
const HELP: &str = "/new  start a new conversation\n/cd <path>  switch to another folder\n/add-dir <path>  open another folder too\n/compact [what to keep]  summarize the conversation so far\n/mode [ask|accept-edits|plan|dont-ask]  show or switch the permission mode\n/review [branch|PR]  review changes (/security-review for security only)\n/<command> [args]  your .gz/commands files and skills\n/exit quit (or Ctrl+D)\nCtrl+C stops the current answer";

pub async fn run(rt: &Runtime, args: &CodeArgs) -> Result<()> {
    let here = std::env::current_dir().context("cannot read the current folder")?;
    let stdin_piped = !std::io::stdin().is_terminal();
    let report_format = matches!(rt.format, OutputFormat::Json | OutputFormat::Yaml);
    let has_prompt = args.prompt.is_some() || (stdin_piped && report_format);
    let headless = code_headless::plan(args, rt.format, has_prompt)?;
    // Bad flags and a missing prompt fail before a worktree is made, and a
    // scripted run reports them in its own format.
    let prepared = async {
        code_headless::controls(args)?;
        let add_dir = code_headless::anchor_add_dirs(&here, &args.add_dir)?;
        let piped = match &headless {
            Some(plan) if plan.reads_piped_prompt() && args.prompt.is_none() => {
                Some(piped_prompt(stdin_piped).await?)
            }
            _ => None,
        };
        let worktree = code_worktree::enter(&here, args.worktree.as_deref())?;
        let args = CodeArgs {
            add_dir,
            ..args.clone()
        };
        anyhow::Ok((args, piped, worktree))
    }
    .await;
    let (args, piped, worktree) = match (prepared, &headless) {
        (Ok(prepared), _) => prepared,
        (Err(err), Some(plan)) => {
            return code_headless::fail_to_start(plan, args.conversation.as_deref(), None, err)
        }
        (Err(err), None) => return Err(err),
    };
    if let (Some(worktree), false) = (&worktree, rt.asked_quiet) {
        eprintln!("{}", format!("· {}", worktree.opening_note()).dimmed());
    }
    let root = worktree
        .as_ref()
        .map_or(here.as_path(), |w| w.start.as_path());
    let ended = start(rt, &args, root, None, piped, headless).await;
    if let (Some(worktree), false) = (&worktree, rt.asked_quiet) {
        // On stderr, so a scripted run's stdout stays its result.
        eprintln!("{}", worktree.closing_note());
    }
    ended
}

/// A one-shot run's prompt.
enum OneShot<'a> {
    /// The prompt argument; `@file` reads that file.
    Argument(&'a str),
    /// Piped in on standard input, taken as it is.
    Piped(String),
}

impl OneShot<'_> {
    fn text(&self) -> Result<String> {
        match self {
            OneShot::Argument(raw) => crate::cmd::read_text_arg(raw),
            OneShot::Piped(text) => Ok(text.clone()),
        }
    }
}

/// A scripted run's prompt from standard input: with no prompt argument,
/// all of piped input is the prompt (a plain piped run reads prompts line
/// by line instead).
async fn piped_prompt(stdin_piped: bool) -> Result<String> {
    if !stdin_piped {
        anyhow::bail!(
            "gz code -p needs a prompt: pass it as an argument (gz code -p \"fix the test\"), \
             from a file (gz code -p @prompt.txt) or piped in (echo \"fix the test\" | gz code -p)"
        );
    }
    let text = tokio::task::spawn_blocking(|| {
        let mut text = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut text).map(|_| text)
    })
    .await
    .context("cannot read the prompt from standard input")?
    .context("cannot read the prompt from standard input")?;
    let text = text.trim();
    if text.is_empty() {
        anyhow::bail!(
            "standard input was empty, so the run has no prompt; pass one as an argument \
             (gz code -p \"fix the test\"), from a file (gz code -p @prompt.txt) \
             or piped in (echo \"fix the test\" | gz code -p)"
        );
    }
    Ok(text.to_string())
}

/// Opens the interactive gz code in `root` with `draft` typed into the input
/// box, so the user reads (and may edit) it before pressing Enter to send.
/// Needs a terminal: a draft is never sent unseen.
pub async fn run_with_draft(rt: &Runtime, root: &std::path::Path, draft: String) -> Result<()> {
    let args = CodeArgs::default();
    start(rt, &args, root, Some(draft), None, None).await
}

async fn start(
    rt: &Runtime,
    args: &CodeArgs,
    root: &std::path::Path,
    draft: Option<String>,
    piped: Option<String>,
    headless: Option<code_headless::Plan>,
) -> Result<()> {
    let one_shot = match (&args.prompt, piped) {
        (Some(raw), _) => Some(OneShot::Argument(raw)),
        (None, Some(text)) => Some(OneShot::Piped(text)),
        (None, None) => None,
    };
    let mut session = Session::new(rt.http.clone(), root)?;
    session.set_local_mcp(Some(crate::assistant::mcp_local::LocalMcp::open_reporting(
        root,
    )));
    session.set_subagents(rt.http.resolved().profile.code_subagents.unwrap_or(true));
    session.set_auto_compact(auto_compact_setting(rt, args)?);
    session.set_subagent_settings(
        rt.http
            .resolved()
            .profile
            .code_subagent_settings
            .clone()
            .unwrap_or_default(),
    );
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
            .and_then(|index| index.last(root));
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
    if args.fork_session {
        let picks = args.conversation.is_some() || args.continue_last || args.resume;
        match crate::code_ui::sessions::fork_at_start(&mut session, &rt.http, start, picks).await {
            Ok(forked) => start = forked,
            Err(err) => {
                return match &headless {
                    Some(plan) => code_headless::fail_to_start(
                        plan,
                        Some(session.conversation_id()),
                        None,
                        err,
                    ),
                    None => Err(err),
                }
            }
        }
    }
    let interactive = one_shot.is_none()
        && headless.is_none()
        && std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
        && rt.format == OutputFormat::Table;
    if !args.bare {
        session.set_hooks(load_hooks(root, interactive));
        session.set_diagnostics(crate::assistant::diagnostics::Diagnostics::load());
    }
    let controls = code_headless::controls(args)?;
    if let Err(err) = code_headless::apply_flags(&mut session, args, controls.clone()) {
        return match &headless {
            Some(plan) => {
                code_headless::fail_to_start(plan, Some(session.conversation_id()), None, err)
            }
            None => Err(err),
        };
    }
    let mode = match crate::cmd::code_rules::setup(&mut session, args, interactive) {
        Ok(mode) => mode,
        Err(err) => {
            return match &headless {
                Some(plan) => {
                    code_headless::fail_to_start(plan, Some(session.conversation_id()), None, err)
                }
                None => Err(err),
            };
        }
    };
    if draft.is_some() && !interactive {
        anyhow::bail!("gz code needs an interactive terminal to show the prepared message; use --print to see it instead");
    }
    // The interactive UI draws at once and loads the model list in the
    // background; an explicit --model/--effort is validated up front.
    let explicit = args.model.is_some() || args.effort.is_some();
    // Scripted runs name the model they ran on, and one-shot auto mode
    // checks the list for the server's action review.
    let load_list = interactive
        || headless.is_some()
        || (one_shot.is_some() && mode == crate::code_ui::approver::Mode::Auto);
    let chosen = if interactive && !explicit {
        Ok((None, None))
    } else {
        choose_model(rt, args, load_list).await
    };
    let (catalog, selection) = match (chosen, &headless) {
        (Ok(chosen), _) => chosen,
        (Err(err), Some(plan)) => {
            return code_headless::fail_to_start(plan, Some(session.conversation_id()), None, err)
        }
        (Err(err), None) => return Err(err),
    };
    session.set_model(selection.clone());
    // A run that can't ask follows the saved price choice for its model; the
    // interactive UI settles it turn by turn.
    if !interactive && session.auto_compact().is_none() {
        let runs_on = selection
            .clone()
            .or_else(|| catalog.as_ref()?.default_selection());
        session.set_auto_compact(saved_standard_price_threshold(
            &rt.http.resolved().profile,
            runs_on.as_ref().map(|selection| &selection.model),
        ));
    }
    if let Some(catalog) = &catalog {
        use_subagent_features(&mut session, catalog);
    }
    // One-shot and piped runs skip the model list; local context only needs
    // the capabilities, read before the first turn goes out (at once when
    // the flags need them).
    let mut probe = catalog.is_none() && !interactive;
    let model = selection
        .clone()
        .or_else(|| catalog.as_ref()?.default_selection())
        .map(|selection| selection.model.key());
    // Appended instructions need local context, so even the interactive UI
    // asks the server before it opens rather than dropping them.
    let read_now = catalog.is_none() && (controls.needs_local_context() || headless.is_some());
    let checked = async {
        if read_now {
            probe = false;
            code_headless::read_local_context(&mut session, &controls).await?;
        }
        code_headless::check_server_takes(&session, &controls)
    }
    .await;
    if let Err(err) = checked {
        return match &headless {
            Some(plan) => {
                code_headless::fail_to_start(plan, Some(session.conversation_id()), model, err)
            }
            None => Err(err),
        };
    }
    let mid_line = Arc::new(AtomicBool::new(false));
    let mut spend = PlainSpend {
        meter: Meter::new(
            rt.http
                .resolved()
                .profile
                .code_spend_caps
                .unwrap_or_default(),
            Ledger::open(&rt.http.resolved().profile_name).ok(),
        ),
        model: selection
            .clone()
            .or_else(|| catalog.as_ref()?.default_selection())
            .map(|s| s.model.label()),
        steps: Tally::default(),
    };
    if let Some(plan) = headless {
        warn_if_unconfined(&session, rt.asked_quiet);
        let prompt = match one_shot.as_ref().map(OneShot::text).transpose() {
            Ok(prompt) => prompt,
            Err(err) => {
                return code_headless::fail_to_start(
                    &plan,
                    Some(session.conversation_id()),
                    model,
                    err,
                )
            }
        };
        let run = code_headless::Run {
            rt,
            args,
            catalog: catalog.as_ref(),
            model,
            prompt,
            controls,
            mid_line,
        };
        return code_headless::run(plan, run, &mut session, &mut spend).await;
    }
    if let Some(one_shot) = &one_shot {
        warn_if_unconfined(&session, rt.asked_quiet);
        let ended = async {
            let typed =
                crate::cmd::code_slash::expand(&session, catalog.as_ref(), &one_shot.text()?)?;
            if let Some(model) = typed.model {
                spend.price_as(Some(model.model.label()));
                session.set_model(Some(model));
            }
            let prompt = typed.prompt;
            let approver = one_shot_approver(
                rt,
                args,
                &session,
                catalog.as_ref(),
                &prompt,
                None,
                &mid_line,
            )?;
            spend.check()?;
            turn(
                &mut session,
                &prompt,
                approver.as_ref(),
                &mid_line,
                &mut probe,
                &mut spend,
            )
            .await
        }
        .await;
        crate::code_ui::hooks::on_exit(&session.hooks(), &session.sandbox().cwd()).await;
        return match ended? {
            TurnEnd::Failed(why) => anyhow::bail!("{why}"),
            _ => Ok(()),
        };
    }
    if interactive {
        let resolved = rt.http.resolved();
        let identity = banner_identity(resolved);
        let models = match catalog {
            Some(catalog) => crate::code_ui::Models::Ready(Box::new(catalog), selection),
            None => crate::code_ui::Models::Loading(spawn_model_load(rt, args)),
        };
        let saved = saved_model_hint(&rt.http.resolved().profile);
        return crate::code_ui::run(session, identity, models, saved, start, mode, draft).await;
    }
    let (approver, mode_switch) = crate::cmd::code_rules::switchable(
        mode,
        Box::new(TerminalApprover {
            mid_line: mid_line.clone(),
        }),
    );
    let approver = approver.as_ref();
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
            command if command.split_whitespace().next() == Some("/compact") => {
                compact_plain(&session, command).await
            }
            command if command.split_whitespace().next() == Some("/mode") => {
                let arg = command.trim_start_matches("/mode");
                match crate::cmd::code_rules::mode_command(&mode_switch, arg) {
                    Ok(text) => eprintln!("{}", format!("· {text}").dimmed()),
                    Err(err) => crate::output::warn(&format!("{err:#}")),
                }
            }
            command if command.starts_with("/cd ") || command.starts_with("/add-dir ") => {
                let (name, path) = command.split_once(' ').unwrap_or((command, ""));
                let done = if name == "/cd" {
                    session.change_dir(path.trim())
                } else {
                    session.add_dir(path.trim())
                };
                match done {
                    Ok(text) => eprintln!("{}", format!("· {text}").dimmed()),
                    Err(err) => crate::output::warn(&format!("{err:#}")),
                }
            }
            line => {
                if let Err(err) = spend.check() {
                    crate::output::warn(&format!("{err:#}"));
                    continue;
                }
                let typed = match crate::cmd::code_slash::expand(&session, catalog.as_ref(), line) {
                    Ok(typed) => typed,
                    Err(err) => {
                        crate::output::warn(&format!("{err:#}"));
                        continue;
                    }
                };
                let own_model = typed.model.is_some();
                if own_model {
                    session.set_model(typed.model);
                }
                let ended = turn(
                    &mut session,
                    &typed.prompt,
                    approver,
                    &mid_line,
                    &mut probe,
                    &mut spend,
                )
                .await;
                if own_model {
                    session.set_model(selection.clone());
                }
                if let TurnEnd::Failed(why) = ended? {
                    crate::output::warn(&why);
                }
            }
        }
    }
    crate::code_ui::hooks::on_exit(&session.hooks(), &session.sandbox().cwd()).await;
    eprintln!(
        "{}",
        format!("continue later: gz code -c {}", session.conversation_id()).dimmed()
    );
    Ok(())
}

/// The user's and the project's hooks. Only the interactive UI can ask to
/// trust a project file, so other runs say why its hooks stay off.
fn load_hooks(root: &std::path::Path, interactive: bool) -> crate::assistant::hooks::Hooks {
    use crate::assistant::hooks::{Hooks, Places, ProjectState};
    let hooks = Hooks::load(Places::for_folder(root));
    if !interactive {
        let status = hooks.status();
        for problem in &status.problems {
            crate::output::warn(problem);
        }
        if status.project == Some(ProjectState::Untrusted) {
            crate::output::warn(
                "project hooks in .gz/hooks.toml are off until you trust them: run gz code in a terminal and answer the prompt (or /hooks trust)",
            );
        }
    }
    hooks
}

/// `/compact [focus]` in the plain prompt loop: one line with the result.
async fn compact_plain(session: &Session, command: &str) {
    let focus = command.trim_start_matches("/compact").trim();
    let focus = (!focus.is_empty()).then_some(focus);
    let cwd = Some(session.sandbox().cwd());
    let hooked = crate::code_ui::hooks::before_compact(
        Some(session.hooks()),
        cwd,
        session.conversation_id(),
        focus,
    );
    if let Err(why) = hooked.await {
        return crate::output::warn(&why);
    }
    match compaction::compact(&session.http(), session.conversation_id(), focus).await {
        Ok(report) if report.compacted => eprintln!(
            "{}",
            format!(
                "· compacted {} messages: ~{} → ~{} tokens resent per turn",
                report.summarized_messages,
                crate::code_ui::usage::short(report.before_tokens),
                crate::code_ui::usage::short(report.after_tokens)
            )
            .dimmed()
        ),
        Ok(_) => eprintln!("{}", "· nothing to compact yet".dimmed()),
        Err(err) => crate::output::warn(&format!("couldn't compact the conversation: {err:#}")),
    }
}

/// The model for this session: `--model`/`--effort` first, then the
/// profile's saved choice; `None` means the server default. The list is
/// loaded only when it is needed (always for the interactive picker).
/// Loads the model list off the startup path; resolves to the catalog and
/// the saved choice, or `None` if the list can't be loaded.
fn spawn_model_load(rt: &Runtime, args: &CodeArgs) -> crate::code_ui::ModelLoad {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let http = rt.http.clone();
    let profile_name = rt.http.resolved().profile_name.clone();
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
            if let Some(selection) = &selection {
                // Best effort: losing this only leaves the next welcome box
                // without a model name.
                let _ = refresh_model_label(&profile_name, selection);
            }
            (catalog, selection)
        });
        let _ = tx.send(loaded);
    });
    rx
}

/// The saved model's name and effort (`Kimi K3.1 · max`), shown until the
/// list loads. Nothing when only its id is saved: ids aren't for people.
fn saved_model_hint(profile: &crate::config::Profile) -> Option<String> {
    let model = profile.code_model_label.clone()?;
    Some(match &profile.code_effort {
        Some(effort) => format!("{model} · {effort}"),
        None => model,
    })
}

/// `load_list` loads the list even when nothing was asked for, for those
/// that need it (the interactive picker, script mode, one-shot auto mode).
async fn choose_model(
    rt: &Runtime,
    args: &CodeArgs,
    load_list: bool,
) -> Result<(Option<Catalog>, Option<Selection>)> {
    let profile = &rt.http.resolved().profile;
    let wanted = args.model.clone().or_else(|| profile.code_model.clone());
    let effort = args.effort.clone().or_else(|| profile.code_effort.clone());
    // Saved subagent settings go out only if the server takes them, which
    // the model list says.
    let has_subagent_settings = profile
        .code_subagent_settings
        .as_ref()
        .is_some_and(|s| !s.is_empty());
    if !load_list && wanted.is_none() && effort.is_none() && !has_subagent_settings {
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

/// Saves `/memory on|off` for this profile (on is the default, so it's
/// stored only when off).
pub fn remember_memory(profile: &str, on: bool) -> Result<()> {
    let mut config = crate::config::load_config()?;
    config.profile_mut(profile).code_memory = (!on).then_some(false);
    crate::config::save_config(&config).map(|_| ())
}

/// The auto-compact threshold for this session: `--auto-compact` (saved
/// for next time, like `--model`), else the profile's saved choice.
fn auto_compact_setting(rt: &Runtime, args: &CodeArgs) -> Result<Option<u64>> {
    let Some(text) = &args.auto_compact else {
        return Ok(rt.http.resolved().profile.code_auto_compact);
    };
    let tokens = compaction::parse(text)?;
    remember_auto_compact(&rt.http.resolved().profile_name, tokens)?;
    Ok(tokens)
}

/// The threshold that keeps `model` on its standard price, when the profile
/// says to stay there and the model costs more past a prompt size.
fn saved_standard_price_threshold(
    profile: &crate::config::Profile,
    model: Option<&models::Model>,
) -> Option<u64> {
    if profile.code_long_context != Some(false) {
        return None;
    }
    compaction::standard_price_step(model?.long_context()?.threshold)
}

/// Saves the auto-compact threshold for this profile (`None` clears it).
pub fn remember_auto_compact(profile: &str, tokens: Option<u64>) -> Result<()> {
    let mut config = crate::config::load_config()?;
    config.profile_mut(profile).code_auto_compact = tokens;
    crate::config::save_config(&config).map(|_| ())
}

/// Saves the user's price choice for models with a long-context price.
pub fn remember_price_tier(profile: &str, tier: crate::code_ui::state::PriceTier) -> Result<()> {
    let mut config = crate::config::load_config()?;
    config.profile_mut(profile).code_long_context = Some(tier.to_saved());
    crate::config::save_config(&config).map(|_| ())
}

/// Saves the `/budget` caps for this profile (none set clears them).
pub fn remember_caps(profile: &str, caps: &crate::spend::caps::Caps) -> Result<()> {
    let mut config = crate::config::load_config()?;
    config.profile_mut(profile).code_spend_caps = (!caps.is_empty()).then_some(*caps);
    crate::config::save_config(&config).map(|_| ())
}

/// Saves the `/subagents` settings for this profile (none set clears them).
pub fn remember_subagent_settings(profile: &str, settings: &SubagentSettings) -> Result<()> {
    let mut config = crate::config::load_config()?;
    config.profile_mut(profile).code_subagent_settings =
        (!settings.is_empty()).then(|| settings.clone());
    crate::config::save_config(&config).map(|_| ())
}

/// Sends the saved subagent settings only if the server takes them, minus
/// a model it no longer offers.
fn use_subagent_features(session: &mut Session, catalog: &Catalog) {
    session.set_subagent_settings_supported(catalog.subagent_settings.is_some());
    session.set_local_context(catalog.local_context);
    let mut settings = session.subagent_settings().clone();
    if let Some(note) = settings.drop_unavailable(catalog) {
        crate::output::warn(&note);
        session.set_subagent_settings(settings);
    }
}

pub fn remember_model(profile: &str, selection: &Selection) -> Result<()> {
    let mut config = crate::config::load_config()?;
    let entry = config.profile_mut(profile);
    entry.code_model = Some(selection.model.key());
    entry.code_model_label = Some(selection.model.label());
    entry.code_effort = selection.effort.clone();
    crate::config::save_config(&config).map(|_| ())
}

/// Saves the display name of the profile's saved model when the list has a
/// new one: a choice saved before names were kept, or a renamed model.
fn refresh_model_label(profile: &str, selection: &Selection) -> Result<()> {
    let mut config = crate::config::load_config()?;
    let entry = config.profile_mut(profile);
    let label = selection.model.label();
    let is_saved_model = entry.code_model.as_deref() == Some(selection.model.key().as_str());
    if !is_saved_model || entry.code_model_label.as_deref() == Some(label.as_str()) {
        return Ok(());
    }
    entry.code_model_label = Some(label);
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

/// Tells a one-shot run's user, on stderr so script mode's stdout stays
/// clean, that the model's commands run without OS limits here.
fn warn_if_unconfined(session: &Session, quiet: bool) {
    let settings = session.local_context().os_sandbox.settings();
    if let Some(note) = crate::assistant::os_sandbox::unconfined_notice(&settings) {
        if !quiet {
            crate::output::warn(&note);
        }
    }
}

/// Who decides a one-shot run's actions that nothing allowed yet: the
/// terminal (which refuses without one), or in script mode no one; with
/// `--auto`, auto mode's review first. Script mode records each decision.
pub(crate) fn one_shot_approver(
    rt: &Runtime,
    args: &CodeArgs,
    session: &Session,
    catalog: Option<&Catalog>,
    prompt: &str,
    decisions: Option<DecisionLog>,
    mid_line: &Arc<AtomicBool>,
) -> Result<Box<dyn Approver>> {
    let person: Box<dyn Approver> = match decisions {
        Some(_) => Box::new(Nobody),
        None => Box::new(TerminalApprover {
            mid_line: mid_line.clone(),
        }),
    };
    let mode = crate::cmd::code_rules::start_mode(args, &session.rules());
    let approver: Box<dyn Approver> = if mode == crate::code_ui::approver::Mode::Auto {
        Box::new(AutoApprover::new(
            &rt.http,
            session.sandbox(),
            catalog,
            prompt,
            person,
        )?)
    } else {
        person
    };
    let approver = crate::cmd::code_rules::apply_mode(mode, approver);
    Ok(match decisions {
        Some(log) => Box::new(Recording::new(approver, log)),
        None => approver,
    })
}

/// Runs one turn; Ctrl+C cancels it on the server and returns to the
/// prompt. With `probe` set, the server's capabilities are read first (once).
async fn turn(
    session: &mut Session,
    prompt: &str,
    approver: &dyn Approver,
    mid_line: &Arc<AtomicBool>,
    probe: &mut bool,
    spend: &mut PlainSpend,
) -> Result<TurnEnd> {
    let prepared = session.prepare(prompt, &[])?;
    if std::mem::take(probe) {
        let limits = crate::assistant::local_context::probe(&session.http()).await;
        session.set_local_context(limits);
    }
    for note in &prepared.notes {
        eprintln!("{}", format!("  ⎿  {note}").dimmed());
    }
    let canceller = session.canceller();
    let mut subagents = Board::default();
    let mut render = |event: AssistantEvent| {
        match &event {
            AssistantEvent::Usage(usage) => {
                spend.reported(usage, mid_line);
            }
            AssistantEvent::StepUsage(tokens) => spend.step(tokens),
            _ => {}
        }
        render_event(event, mid_line, &mut subagents)
    };
    crate::timing::start_turn();
    let send = session.send_prepared(&prepared, approver, &mut render);
    tokio::pin!(send);
    let end = tokio::select! {
        end = &mut send => end?,
        _ = tokio::signal::ctrl_c() => {
            // The turn stops its commands and prompts, answers the run's
            // open calls as interrupted and cancels it; give it a moment.
            canceller.interrupt();
            tokio::time::timeout(INTERRUPT_CLEANUP, send).await.ok();
            TurnEnd::Cancelled
        }
    };
    end_line(mid_line);
    if end == TurnEnd::Cancelled {
        eprintln!("{}", "· stopped".dimmed());
    }
    Ok(end)
}

/// Spend in the plain modes: a turn is refused once a cap is spent (there
/// is nobody to ask), and each reported cost goes to the ledger.
/// A whole turn's usage, as [`PlainSpend::reported`] works it out.
pub(crate) struct TurnCost {
    /// Every leg's tokens; as cost, the reported one plus `earlier`'s
    /// estimate.
    pub(crate) usage: Usage,
    pub(crate) earlier: EarlierLegs,
}

/// The legs of a turn before its last, which the server never prices: each
/// client-tools resume is its own run and only the last run's cost comes
/// back.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct EarlierLegs {
    /// Their cost, priced at the rate the last leg showed: an estimate.
    pub(crate) estimated_usd: f64,
    /// Legs no rate was known for, so their cost is missing.
    pub(crate) unpriced: usize,
}

pub(crate) struct PlainSpend {
    meter: Meter,
    /// The model turns run on, for the ledger.
    model: Option<String>,
    /// The current run's steps, until its report prices them.
    steps: Tally,
}

impl PlainSpend {
    pub(crate) fn check(&mut self) -> Result<()> {
        self.meter.catch_up();
        match self.meter.guard.reached() {
            Some(usage) => Err(CapReached(usage).into()),
            None => Ok(()),
        }
    }

    /// Prices and records the next turns as `model` (a label), returning
    /// the model they were priced as before.
    pub(crate) fn price_as(&mut self, model: Option<String>) -> Option<String> {
        std::mem::replace(&mut self.model, model)
    }

    /// One step (run leg) of the turn finished with these tokens.
    pub(crate) fn step(&mut self, tokens: &TokenCounts) {
        self.steps.step(tokens);
    }

    /// A run's report (its inline subagents' cost included). The reported
    /// cost covers only the run's last leg, so the rate is learned from that
    /// leg and the earlier legs are priced at it, as the full-screen UI
    /// does. Returns the whole turn's usage: every leg's tokens, and as cost
    /// everything charged (the reported cost plus that estimate), with how
    /// much of it is estimated.
    pub(crate) fn reported(&mut self, usage: &Usage, mid_line: &AtomicBool) -> TurnCost {
        let earlier = self.steps.earlier_steps(usage);
        let whole = Usage {
            tokens: self.steps.turn_tokens(usage),
            ..usage.clone()
        };
        self.steps = Tally::default();
        let Some(cost) = usage.cost_usd else {
            return TurnCost {
                usage: whole,
                earlier: EarlierLegs::default(),
            };
        };
        let model = self.model.clone();
        let mut rate = self.meter.rate(model.as_deref());
        if let Some(tokens) = &usage.tokens {
            if let Some(learned) = rates::learn(rate, cost, tokens) {
                self.meter.keep_rate(model.as_deref(), learned);
                rate = Some(learned);
            }
        }
        self.charge(cost, mid_line);
        let earlier = match (earlier, rate) {
            (Some((_, tokens)), Some(rate)) => EarlierLegs {
                estimated_usd: rates::estimate(rate, &tokens),
                unpriced: 0,
            },
            (Some((legs, _)), None) => EarlierLegs {
                estimated_usd: 0.0,
                unpriced: legs,
            },
            (None, _) => EarlierLegs::default(),
        };
        if earlier.estimated_usd > 0.0 {
            self.charge(earlier.estimated_usd, mid_line);
        }
        TurnCost {
            usage: Usage {
                cost_usd: Some(cost + earlier.estimated_usd),
                ..whole
            },
            earlier,
        }
    }

    fn charge(&mut self, usd: f64, mid_line: &AtomicBool) {
        let recorded = self.meter.record(usd, self.model.as_deref());
        if let Some(why) = recorded.ledger_error {
            note(mid_line, format!("couldn't save today's spend: {why}"));
        }
        for alert in recorded.alerts {
            note(mid_line, format!("{}", alert.message().yellow()));
        }
    }
}

pub(crate) fn end_line(mid_line: &AtomicBool) {
    if mid_line.swap(false, Ordering::SeqCst) {
        println!();
    }
}

fn note(mid_line: &AtomicBool, text: String) {
    end_line(mid_line);
    eprintln!("{text}");
}

pub(crate) fn render_event(event: AssistantEvent, mid_line: &AtomicBool, subagents: &mut Board) {
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
        }
        | AssistantEvent::ServerToolFinished {
            is_error, detail, ..
        } => {
            let mark = if is_error {
                "✗".red().to_string()
            } else {
                "✓".green().to_string()
            };
            note(mid_line, format!("  {mark} {}", detail.dimmed()));
        }
        AssistantEvent::Reconnecting(why) => {
            note(mid_line, format!("{}", format!("⚠ {why}").dimmed()))
        }
        AssistantEvent::Notice(text) | AssistantEvent::Folders(Ok(text)) => {
            note(mid_line, format!("{}", format!("· {text}").dimmed()))
        }
        AssistantEvent::Folders(Err(why)) => note(mid_line, format!("{} {why}", "✗".red())),
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
        AssistantEvent::Usage(_)
        | AssistantEvent::StepUsage(_)
        | AssistantEvent::Context(_)
        | AssistantEvent::SteerSent(_)
        | AssistantEvent::SteerApplied(_)
        | AssistantEvent::SteerUnapplied(_) => {}
        // The plain REPL runs subagents inline; it never asks for background ones.
        AssistantEvent::SubagentRequested(_) | AssistantEvent::SubagentReport(_) => {}
        // The plain REPL doesn't offer todo_write.
        AssistantEvent::Todos(_) => {}
        // Reasoning stays in the full-screen view, folded; plain output
        // prints the answer only.
        AssistantEvent::Thinking(_) => {}
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
                Ok(Ok(line)) => answer_to(request, &line),
                _ => Decision::Deny,
            }
        })
    }
}

/// `  [y] once · [c] this chat (…) · [a] always (…) · [n] deny › `; the
/// always choice is left out when the request can't offer it.
fn repl_choices(request: &PermissionRequest) -> String {
    let mut parts = vec![format!("{} once", "[y]".cyan())];
    // An ask rule offers neither: it asks every time.
    let remembers = !request.chat_scope.is_empty();
    if remembers {
        parts.push(format!(
            "{} this chat ({})",
            "[c]".cyan(),
            request.chat_scope
        ));
    }
    match &request.always_scope {
        Some(scope) => parts.push(format!("{} always ({scope})", "[a]".cyan())),
        None if !remembers => {}
        None => parts.push(
            "always isn't available for destructive actions"
                .dimmed()
                .to_string(),
        ),
    }
    parts.push(format!("{} deny", "[n]".cyan()));
    format!("  {} › ", parts.join(" · "))
}

/// The decision `line` gives for `request`, counting only offered answers.
fn answer_to(request: &PermissionRequest, line: &str) -> Decision {
    match parse_decision(line, request.always_scope.is_some()) {
        Decision::AllowChat if request.chat_scope.is_empty() => Decision::Deny,
        decision => decision,
    }
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

/// Who the welcome box says you are. The site is its name, `GZ_SITE`
/// included, never its web address (that can be a local dev URL).
fn banner_identity(resolved: &crate::config::Resolved) -> crate::code_ui::Identity {
    crate::code_ui::Identity {
        site: resolved.site(),
        user: resolved.profile.username.clone(),
        profile: resolved.profile_name.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_welcome_box_names_a_site_set_only_through_gz_site() {
        let config = crate::config::Config::default();
        let mut resolved = crate::config::resolve(&config, Some("t"), None, None);
        resolved.profile.site = None;
        resolved.profile.web_url = Some("http://localhost:3000".into());
        std::env::set_var("GZ_SITE", "acme-banner-test");
        let identity = banner_identity(&resolved);
        std::env::remove_var("GZ_SITE");
        assert_eq!(identity.site.as_deref(), Some("acme-banner-test"));
    }

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
                context_window: None,
                long_context: None,
            }),
            effort: Some("high".into()),
        };
        remember_model("p", &selection).unwrap();
        let mut config = crate::config::load_config().unwrap();
        config.profile_mut("saved-earlier").code_model = Some("opus".into());
        config.profile_mut("other-model").code_model = Some("haiku55".into());
        crate::config::save_config(&config).unwrap();
        refresh_model_label("saved-earlier", &selection).unwrap();
        refresh_model_label("other-model", &selection).unwrap();
        let config = crate::config::load_config().unwrap();
        std::env::remove_var("XDG_CONFIG_HOME");
        let profile = config.profile("p");
        assert_eq!(profile.code_model.as_deref(), Some("opus"));
        assert_eq!(profile.code_model_label.as_deref(), Some("Claude Opus 5"));
        assert_eq!(profile.code_effort.as_deref(), Some("high"));
        assert_eq!(
            config.profile("saved-earlier").code_model_label.as_deref(),
            Some("Claude Opus 5"),
            "named once the list loads"
        );
        assert_eq!(config.profile("other-model").code_model_label, None);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_run_that_cannot_ask_follows_the_saved_price_choice() {
        use crate::assistant::models::{GroundzeroMode, LongContextPricing, Model};
        let mode = |long_context| {
            Model::Groundzero(GroundzeroMode {
                value: "m".into(),
                label: "M".into(),
                efforts: Vec::new(),
                context_window: Some(868_928),
                long_context,
            })
        };
        let two_prices = mode(Some(LongContextPricing {
            threshold: 272_000,
            input_percent: Some(200),
            output_percent: Some(150),
        }));
        let profile = |code_long_context| crate::config::Profile {
            code_long_context,
            ..Default::default()
        };
        let threshold =
            |saved, model: &Model| saved_standard_price_threshold(&profile(saved), Some(model));
        assert_eq!(threshold(Some(false), &two_prices), Some(200_000));
        assert_eq!(threshold(Some(true), &two_prices), None, "full window");
        assert_eq!(threshold(None, &two_prices), None, "never chose");
        assert_eq!(threshold(Some(false), &mode(None)), None, "one price");
    }

    #[test]
    fn the_welcome_box_shows_a_models_name_never_its_id() {
        let mut profile = crate::config::Profile {
            code_model: Some("kimi31".into()),
            code_effort: Some("max".into()),
            ..Default::default()
        };
        assert_eq!(saved_model_hint(&profile), None);
        profile.code_model_label = Some("Kimi K3.1".into());
        assert_eq!(
            saved_model_hint(&profile).as_deref(),
            Some("Kimi K3.1 · max")
        );
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
    fn an_ask_rules_prompt_offers_only_once_or_no() {
        let mut request = PermissionRequest {
            access: Access::Command,
            summary: "run_command git push".into(),
            preview: None,
            chat_scope: String::new(),
            always_scope: None,
            action: None,
            asked_by: None,
            target: None,
            note: None,
        };
        let choices = repl_choices(&request);
        assert!(choices.contains("once") && choices.contains("deny"));
        assert!(
            !choices.contains("this chat") && !choices.contains("always"),
            "{choices}"
        );
        assert_eq!(answer_to(&request, "c"), Decision::Deny);
        assert_eq!(answer_to(&request, "y"), Decision::AllowOnce);
        request.chat_scope = "this exact command".into();
        assert!(repl_choices(&request).contains("this chat"));
        assert_eq!(answer_to(&request, "c"), Decision::AllowChat);
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

#[cfg(test)]
mod plain_spend_tests {
    use super::*;
    use crate::spend::caps::Caps;

    fn tokens(input: u64, output: u64) -> TokenCounts {
        TokenCounts {
            input,
            output,
            ..TokenCounts::default()
        }
    }

    fn report(tokens: TokenCounts, cost: f64) -> Usage {
        Usage {
            tokens: Some(tokens),
            cost_usd: Some(cost),
            ..Usage::default()
        }
    }

    #[test]
    fn earlier_steps_of_a_run_are_priced_at_the_last_legs_rate() {
        let dir = std::env::temp_dir().join(format!("gz-plain-spend-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("code_spend.json");
        let _ = std::fs::remove_file(&path);
        let ledger = Ledger::at(path, "test");
        let mut spend = PlainSpend {
            meter: Meter::new(Caps::default(), Some(ledger)),
            model: Some("m".into()),
            steps: Tally::default(),
        };
        let mid_line = AtomicBool::new(false);
        // One leg: the reported cost is the whole run.
        spend.step(&tokens(1_000, 0));
        let turn = spend.reported(&report(tokens(1_000, 0), 0.01), &mid_line);
        assert_eq!(turn.earlier, EarlierLegs::default());
        let whole = turn.usage;
        assert!((whole.cost_usd.unwrap() - 0.01).abs() < 1e-12);
        assert_eq!(whole.tokens, Some(tokens(1_000, 0)));
        // Two legs of the same shape: the report covers only the last.
        spend.step(&tokens(1_000, 0));
        spend.step(&tokens(1_000, 0));
        let turn = spend.reported(&report(tokens(1_000, 0), 0.01), &mid_line);
        assert!((turn.earlier.estimated_usd - 0.01).abs() < 1e-12);
        assert_eq!(turn.earlier.unpriced, 0);
        let whole = turn.usage;
        let charged = whole.cost_usd.unwrap();
        assert!((charged - 0.02).abs() < 1e-12, "{charged}");
        assert_eq!(whole.tokens, Some(tokens(2_000, 0)));
    }

    #[test]
    fn earlier_steps_without_a_known_rate_are_counted_as_unpriced() {
        let mut spend = PlainSpend {
            meter: Meter::new(Caps::default(), None),
            model: Some("unpriced-model".into()),
            steps: Tally::default(),
        };
        let mid_line = AtomicBool::new(false);
        spend.step(&tokens(1_000, 0));
        spend.step(&tokens(1_000, 0));
        // No tokens in the report: no rate can be learned from it.
        let no_tokens = Usage {
            cost_usd: Some(0.01),
            ..Usage::default()
        };
        let turn = spend.reported(&no_tokens, &mid_line);
        assert_eq!(
            turn.earlier,
            EarlierLegs {
                estimated_usd: 0.0,
                unpriced: 1,
            }
        );
        assert_eq!(turn.usage.cost_usd, Some(0.01));
    }
}
