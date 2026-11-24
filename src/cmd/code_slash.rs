//! Slash commands typed outside the full-screen view of `gz code` (a
//! one-shot `gz code "/review"` and the plain line mode): `/review`,
//! `/security-review`, your own command files and `/<skill>` send what
//! they send in the full-screen view.

use anyhow::{bail, Result};

use crate::assistant::models::{Catalog, Selection};
use crate::assistant::Session;
use crate::code_ui::custom_commands;

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
        &session.mentionable_skills(),
    )
    .map_err(anyhow::Error::msg)?;
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
        prompt: expanded.prompt,
        model,
    })
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
