//! `gz code`: the assistant in the terminal. In an interactive terminal it
//! opens the full coding-CLI interface (`crate::code_ui`); one-shot prompts
//! and piped use get plain streamed lines, with permission prompts inline.
//! A one-shot prompt with `-o json`/`-o yaml` is script mode: it never waits
//! for input and prints one report (`crate::cmd::code_report`). Its prompt
//! may also come piped in on standard input.

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
use crate::cmd::code_report::{Report, ReportedExit};
use crate::cmd::code_unattended::{AutoApprover, DecisionLog, Nobody, Recording};
use crate::cmd::Runtime;
use crate::output::OutputFormat;
use crate::spend::caps::CapReached;
use crate::spend::ledger::Ledger;
use crate::spend::meter::Meter;
use crate::subagents::{render as subagent_render, Board, SubagentEvent};

const HELP: &str = "/new  start a new conversation\n/cd <path>  switch to another folder\n/add-dir <path>  open another folder too\n/compact [what to keep]  summarize the conversation so far\n/exit quit (or Ctrl+D)\nCtrl+C stops the current answer";

pub async fn run(rt: &Runtime, args: &CodeArgs) -> Result<()> {
    let root = std::env::current_dir().context("cannot read the current folder")?;
    let piped = piped_prompt(args, rt.format).await?;
    start(rt, args, &root, None, piped).await
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

/// Script mode's prompt from standard input: with `-o json|yaml`, no
/// prompt argument and input piped in, all of it is the prompt (otherwise
/// a piped run reads prompts line by line).
async fn piped_prompt(args: &CodeArgs, format: OutputFormat) -> Result<Option<String>> {
    let is_script_format = matches!(format, OutputFormat::Json | OutputFormat::Yaml);
    if args.prompt.is_some() || !is_script_format || std::io::stdin().is_terminal() {
        return Ok(None);
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
            "standard input was empty, so script mode has no prompt; pass one as an argument \
             (gz code \"fix the test\" -o json), from a file (gz code @prompt.txt -o json) \
             or piped in (echo \"fix the test\" | gz code -o json)"
        );
    }
    Ok(Some(text.to_string()))
}

/// Opens the interactive gz code in `root` with `draft` typed into the input
/// box, so the user reads (and may edit) it before pressing Enter to send.
/// Needs a terminal: a draft is never sent unseen.
pub async fn run_with_draft(rt: &Runtime, root: &std::path::Path, draft: String) -> Result<()> {
    let args = CodeArgs {
        prompt: None,
        yes_reads: false,
        auto: false,
        conversation: None,
        continue_last: false,
        resume: false,
        model: None,
        effort: None,
        auto_compact: None,
    };
    start(rt, &args, root, Some(draft), None).await
}

async fn start(
    rt: &Runtime,
    args: &CodeArgs,
    root: &std::path::Path,
    draft: Option<String>,
    piped: Option<String>,
) -> Result<()> {
    let one_shot = match (&args.prompt, piped) {
        (Some(raw), _) => Some(OneShot::Argument(raw)),
        (None, Some(text)) => Some(OneShot::Piped(text)),
        (None, None) => None,
    };
    let mut session = Session::new(rt.http.clone(), root)?;
    session.set_local_mcp(Some(crate::assistant::mcp_local::LocalMcp::open_reporting()));
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
    let interactive = one_shot.is_none()
        && std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
        && rt.format == OutputFormat::Table;
    session.set_hooks(load_hooks(root, interactive));
    if draft.is_some() && !interactive {
        anyhow::bail!("gz code needs an interactive terminal to show the prepared message; use --print to see it instead");
    }
    let mut script = script_format(one_shot.is_some(), rt.format)?.map(|format| ScriptOutput {
        format,
        quiet: rt.asked_quiet,
        report: Report::new(session.conversation_id(), None, DecisionLog::default()),
    });
    // The interactive UI draws at once and loads the model list in the
    // background; an explicit --model/--effort is validated up front.
    let explicit = args.model.is_some() || args.effort.is_some();
    // Script mode names the model it ran on, and one-shot auto mode checks
    // the list for the server's action review.
    let load_list = interactive || (one_shot.is_some() && (args.auto || script.is_some()));
    let chosen = if interactive && !explicit {
        Ok((None, None))
    } else {
        choose_model(rt, args, load_list).await
    };
    let (catalog, selection) = match (chosen, script.take()) {
        (Ok(chosen), kept) => {
            script = kept;
            chosen
        }
        (Err(err), Some(script)) => return script.finish(Err(err)),
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
    // the capabilities, read before the first turn goes out.
    let mut probe = catalog.is_none() && !interactive;
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
    };
    if let Some(one_shot) = &one_shot {
        warn_if_unconfined(&session, rt.asked_quiet);
        let decisions = script.as_ref().map(|script| script.report.decisions());
        if let Some(script) = &mut script {
            let runs_on = selection
                .clone()
                .or_else(|| catalog.as_ref()?.default_selection());
            script
                .report
                .set_model(runs_on.map(|selection| selection.model.key()));
        }
        let show = match &mut script {
            Some(script) => Show::Script {
                report: &mut script.report,
                quiet: script.quiet,
            },
            None => Show::Plain,
        };
        let ended = async {
            let prompt = one_shot.text()?;
            let approver = one_shot_approver(
                rt,
                args,
                &session,
                catalog.as_ref(),
                &prompt,
                decisions,
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
                show,
            )
            .await
        }
        .await;
        return match script {
            Some(script) => script.finish(ended),
            None => match ended? {
                TurnEnd::Failed(why) => anyhow::bail!("{why}"),
                _ => Ok(()),
            },
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
        let saved = saved_model_hint(&rt.http.resolved().profile);
        let mode = if args.auto {
            crate::code_ui::approver::Mode::Auto
        } else {
            crate::code_ui::approver::Mode::default()
        };
        return crate::code_ui::run(session, identity, models, saved, start, mode, draft).await;
    }
    let approver = TerminalApprover {
        mid_line: mid_line.clone(),
    };
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
            prompt => {
                if let Err(err) = spend.check() {
                    crate::output::warn(&format!("{err:#}"));
                    continue;
                }
                if let TurnEnd::Failed(why) = turn(
                    &mut session,
                    prompt,
                    &approver,
                    &mid_line,
                    &mut probe,
                    &mut spend,
                    Show::Plain,
                )
                .await?
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

/// Where a turn's events go.
enum Show<'a> {
    /// The answer on stdout, everything else on stderr.
    Plain,
    /// Into the report; progress on stderr unless `quiet`, the answer only
    /// in the report.
    Script { report: &'a mut Report, quiet: bool },
}

impl Show<'_> {
    fn is_quiet(&self) -> bool {
        matches!(self, Show::Script { quiet: true, .. })
    }
}

fn is_text(event: &AssistantEvent) -> bool {
    match event {
        AssistantEvent::Text(_) => true,
        AssistantEvent::ForSubagent { event, .. } => is_text(event),
        _ => false,
    }
}

/// Script mode: how to print the report, and the report itself.
struct ScriptOutput {
    format: OutputFormat,
    /// The user's `-q`: no progress on stderr.
    quiet: bool,
    report: Report,
}

impl ScriptOutput {
    /// Prints the report; a run that didn't complete cleanly ends the
    /// process with its exit code (see `code_report::exit`).
    fn finish(self, ended: Result<TurnEnd>) -> Result<()> {
        let summary = self.report.finish(&ended);
        let code = summary.exit_code(&ended);
        if let (false, Some(error)) = (self.quiet, &summary.error) {
            crate::output::warn(error);
        }
        let text = summary.render(self.format)?;
        let mut stdout = std::io::stdout().lock();
        writeln!(stdout, "{}", text.trim_end()).context("cannot write the report")?;
        stdout.flush().context("cannot write the report")?;
        if code == crate::cmd::code_report::exit::COMPLETED {
            return Ok(());
        }
        Err(ReportedExit(code).into())
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

/// `Some(format)` for script mode: a prompt with `-o json` or `-o yaml`.
fn script_format(has_prompt: bool, format: OutputFormat) -> Result<Option<OutputFormat>> {
    if !has_prompt {
        return Ok(None);
    }
    match format {
        OutputFormat::Table => Ok(None),
        OutputFormat::Json | OutputFormat::Yaml => Ok(Some(format)),
        OutputFormat::Csv => {
            anyhow::bail!("gz code reports a run as one object; use -o json or -o yaml")
        }
    }
}

/// Who decides a one-shot run's actions that nothing allowed yet: the
/// terminal (which refuses without one), or in script mode no one; with
/// `--auto`, auto mode's review first. Script mode records each decision.
fn one_shot_approver(
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
    let approver: Box<dyn Approver> = if args.auto {
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
    mut show: Show<'_>,
) -> Result<TurnEnd> {
    let prepared = session.prepare(prompt, &[])?;
    if std::mem::take(probe) {
        let limits = crate::assistant::local_context::probe(&session.http()).await;
        session.set_local_context(limits);
    }
    if !show.is_quiet() {
        for note in &prepared.notes {
            eprintln!("{}", format!("  ⎿  {note}").dimmed());
        }
    }
    let canceller = session.canceller();
    let mut subagents = Board::default();
    let mut render = |event: AssistantEvent| {
        if let AssistantEvent::Usage(usage) = &event {
            spend.reported(usage.cost_usd, mid_line);
        }
        match &mut show {
            Show::Plain => render_event(event, mid_line, &mut subagents),
            Show::Script { report, quiet } => {
                report.observe(&event);
                // The answer goes out once, in the report.
                if !*quiet && !is_text(&event) {
                    render_event(event, mid_line, &mut subagents)
                }
            }
        }
    };
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

/// Spend in the plain modes: a turn is refused once a cap is spent (there
/// is nobody to ask), and each reported cost goes to the ledger.
struct PlainSpend {
    meter: Meter,
    /// The model turns run on, for the ledger.
    model: Option<String>,
}

impl PlainSpend {
    fn check(&mut self) -> Result<()> {
        self.meter.catch_up();
        match self.meter.guard.reached() {
            Some(usage) => Err(CapReached(usage).into()),
            None => Ok(()),
        }
    }

    /// A run's cost (its inline subagents' included).
    fn reported(&mut self, cost: Option<f64>, mid_line: &AtomicBool) {
        let Some(cost) = cost else { return };
        let recorded = self.meter.record(cost, self.model.as_deref());
        if let Some(why) = recorded.ledger_error {
            note(mid_line, format!("couldn't save today's spend: {why}"));
        }
        for alert in recorded.alerts {
            note(mid_line, format!("{}", alert.message().yellow()));
        }
    }
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
        | AssistantEvent::Context(_)
        | AssistantEvent::SteerSent(_)
        | AssistantEvent::SteerApplied(_)
        | AssistantEvent::SteerUnapplied(_) => {}
        // The plain REPL runs subagents inline; it never asks for background ones.
        AssistantEvent::SubagentRequested(_) | AssistantEvent::SubagentReport(_) => {}
        // The plain REPL doesn't offer todo_write.
        AssistantEvent::Todos(_) => {}
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
    fn colors_diff_lines() {
        let text = render_preview(Access::Write, "--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old\n+new");
        assert!(text.contains("old") && text.contains("new"));
        assert!(render_preview(Access::Command, "ls").contains("ls"));
        let script = render_preview(Access::Script, "import gz\n# note\nprint(1)");
        assert!(script.contains("   3 │") && script.contains("print(1)"));
    }
}
