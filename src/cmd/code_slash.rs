//! Slash commands typed outside the full-screen view of `gz code` (a
//! one-shot `gz code "/review"` and the plain line mode): `/review`,
//! `/security-review`, your own command files and `/<skill>` send what
//! they send in the full-screen view.

use anyhow::{bail, Result};

use crate::assistant::models::{Catalog, Selection};
use crate::assistant::turn_marks::CommandMark;
use crate::assistant::Session;
use crate::code_ui::custom_commands::{self, Expanded, ExpandedKind};

/// The message to send for a typed line, and the model a command file
/// asked for (`None`: the session's).
pub struct Typed {
    pub prompt: String,
    pub model: Option<Selection>,
}

/// Expands `line` when it is one of those commands; otherwise it goes as
/// typed. `catalog` resolves a command file's `model:`.
pub fn expand(session: &Session, catalog: Option<&Catalog>, line: &str) -> Result<Typed> {
    let config = crate::config::config_path().ok();
    let user_config = config.as_deref().and_then(std::path::Path::parent);
    let expanded = custom_commands::expand_line(
        line,
        &session.sandbox().start(),
        user_config,
        &session.installed_skills(),
    )
    .map_err(anyhow::Error::msg)?;
    if let Some(expanded) = &expanded {
        check_offered(session, line, expanded)?;
    }
    let Some(expanded) = expanded else {
        return Ok(Typed {
            prompt: line.to_string(),
            model: None,
        });
    };
    let model = match &expanded.model {
        None => None,
        Some(wanted) => Some(resolve(catalog, wanted, &expanded.source)?),
    };
    Ok(Typed {
        prompt: CommandMark::wrap(line, &expanded.prompt),
        model,
    })
}

/// Refuses a command file or skill this run leaves out, rather than send
/// the line as typed or ask for a tool the model wasn't given.
fn check_offered(session: &Session, line: &str, expanded: &Expanded) -> Result<()> {
    let name = line.split_whitespace().next().unwrap_or(line);
    match expanded.kind {
        ExpandedKind::Review => Ok(()),
        ExpandedKind::CommandFile if session.is_bare() => bail!(
            "{name} is the command file {}, and --bare skips command files; run without --bare to use it",
            expanded.source
        ),
        ExpandedKind::Skill if session.is_bare() => bail!(
            "{name} is one of your skills, and --bare skips skills; run without --bare to use it"
        ),
        ExpandedKind::Skill if !session.mentionable_skills().iter().any(|s| format!("/{s}") == name) => bail!(
            "{name} is one of your skills, and this run doesn't offer load_skill; add load_skill to --tools to use it"
        ),
        ExpandedKind::CommandFile | ExpandedKind::Skill => Ok(()),
    }
}

fn resolve(catalog: Option<&Catalog>, wanted: &str, source: &str) -> Result<Selection> {
    let Some(catalog) = catalog else {
        bail!(
            "{source} asks for the model '{wanted}', and this run did not load the model list; pass --model {wanted} or run it in gz code's full-screen view"
        );
    };
    match catalog.resolve(wanted, None) {
        Some(selection) => Ok(selection),
        None => bail!(
            "{source} asks for the model '{wanted}', which is not available here; fix `model:` in it (/models in gz code's full-screen view lists them)"
        ),
    }
}
