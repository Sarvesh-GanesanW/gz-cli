//! `gz approvals`: what gz may do without asking, in this folder and in
//! chat, and revoking any of it.

use anyhow::{Context, Result};
use serde_json::json;

use crate::assistant::grants;
use crate::cli::ApprovalsAction;
use crate::cmd::Runtime;
use crate::output::{self, OutputFormat};

pub async fn run(rt: &Runtime, action: &ApprovalsAction) -> Result<()> {
    match action {
        ApprovalsAction::List { conversation, all } => {
            let listing = load(rt, conversation.as_deref(), *all).await?;
            if rt.format != OutputFormat::Table {
                let rows: Vec<_> = listing
                    .entries()
                    .into_iter()
                    .map(|e| json!({"id": e.id, "scope": e.scope, "what": e.text}))
                    .collect();
                return rt.show(json!(rows)).await;
            }
            for line in listing.report() {
                println!("{line}");
            }
            Ok(())
        }
        ApprovalsAction::Revoke {
            id,
            conversation,
            all,
        } => {
            let listing = load(rt, conversation.as_deref(), *all).await?;
            let revoked = grants::revoke(&rt.http, &listing, id).await?;
            output::success(&format!("Revoked {}: {revoked}", id.to_ascii_uppercase()));
            Ok(())
        }
    }
}

async fn load(rt: &Runtime, conversation: Option<&str>, all: bool) -> Result<grants::Listing> {
    let here = std::env::current_dir().context("cannot read the current folder")?;
    let here = here.canonicalize().unwrap_or(here);
    let root = (!all).then_some(here.as_path());
    grants::load(&rt.http, root, conversation).await
}
