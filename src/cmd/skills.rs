//! `gz skills` and `gz plugins`: local skill management. Nothing here talks
//! to Groundzero; git sources are fetched with git itself (hardened, see
//! `skills::source`), and every install is previewed and confirmed.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::IsTerminal;

use crate::cli::{ConsentArgs, PluginsAction, SkillFoldersAction, SkillsAction};
use crate::cmd::{prompt_line, Runtime};
use crate::output::{self, OutputFormat};
use crate::skills::files::{human_size, FileFinding};
use crate::skills::install::{self, Consent, Plan, Request};
use crate::skills::source::Source;
use crate::skills::{self, Catalog, Health, Home};

pub async fn run(rt: &Runtime, action: &SkillsAction) -> Result<()> {
    let home = Home::open()?;
    match action {
        SkillsAction::List => list(rt, &home),
        SkillsAction::Show { name } => show(rt, &home, name).await,
        SkillsAction::Install {
            source,
            skills,
            replace,
            consent,
        } => {
            let request = Request {
                source: Source::parse(source)?,
                only: skills.clone(),
                replace: *replace,
            };
            install_confirmed(&home, &request, *consent)
        }
        SkillsAction::Update { name, consent } => {
            for request in install::update_requests(&home, name.as_deref())? {
                install_confirmed(&home, &request, *consent)?;
            }
            Ok(())
        }
        SkillsAction::Remove { name, yes } => {
            crate::cmd::confirm(*yes, &format!("Remove the skill {name}?"))?;
            install::remove(&home, name)?;
            output::success(&format!("Removed {name}"));
            Ok(())
        }
        SkillsAction::Enable { name } => {
            output::success(&skills::set_enabled(&home, name, true)?);
            Ok(())
        }
        SkillsAction::Disable { name } => {
            output::success(&skills::set_enabled(&home, name, false)?);
            Ok(())
        }
        SkillsAction::Verify { accept, yes } => match accept {
            Some(name) => accept_changed(&home, name, *yes),
            None => verify(rt, &home),
        },
        SkillsAction::Folders { action } => folders(rt, &home, action.as_ref()),
    }
}

pub async fn plugins(rt: &Runtime, action: &PluginsAction) -> Result<()> {
    let home = Home::open()?;
    match action {
        PluginsAction::List => {
            let rows: Vec<Value> = bundles(&home)?
                .into_iter()
                .map(|(bundle, entries)| {
                    let first = &entries[0].1;
                    json!({
                        "bundle": bundle,
                        "skills": entries.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(", "),
                        "source": first.source,
                        "commit": first.commit.as_deref().map(|c| &c[..c.len().min(12)]),
                        "installedAt": first.installed_at,
                    })
                })
                .collect();
            if rows.is_empty() && rt.format == OutputFormat::Table {
                println!("No plugins installed. gz plugins install <git URL or folder>");
                return Ok(());
            }
            rt.show_columns(
                &json!(rows),
                &["bundle", "skills", "source", "commit", "installedAt"],
            )
        }
        PluginsAction::Install {
            source,
            replace,
            consent,
        } => {
            let request = Request {
                source: Source::parse(source)?,
                only: None,
                replace: *replace,
            };
            install_confirmed(&home, &request, *consent)
        }
        PluginsAction::Remove { bundle, yes } => {
            let all = bundles(&home)?;
            let Some(entries) = all.get(bundle) else {
                bail!(
                    "no installed bundle '{bundle}'; installed: {}",
                    all.keys().cloned().collect::<Vec<_>>().join(", ")
                );
            };
            let names: Vec<&str> = entries.iter().map(|(n, _)| n.as_str()).collect();
            crate::cmd::confirm(
                *yes,
                &format!("Remove the bundle {bundle} ({})?", names.join(", ")),
            )?;
            for name in &names {
                install::remove(&home, name)?;
            }
            output::success(&format!("Removed {bundle}: {}", names.join(", ")));
            Ok(())
        }
    }
}

type Bundles = BTreeMap<String, Vec<(String, skills::lock::LockEntry)>>;

/// Installed skills grouped by the bundle they came with.
fn bundles(home: &Home) -> Result<Bundles> {
    let mut out: Bundles = BTreeMap::new();
    for (name, entry) in home.lock()?.entries() {
        out.entry(entry.bundle.clone())
            .or_default()
            .push((name.clone(), entry.clone()));
    }
    Ok(out)
}

fn list(rt: &Runtime, home: &Home) -> Result<()> {
    let catalog = Catalog::load(home)?;
    for warning in &catalog.warnings {
        output::warn(warning);
    }
    if catalog.skills.is_empty() && rt.format == OutputFormat::Table {
        println!(
            "No skills yet. Add folders to {} or run `gz skills install <git URL or folder>`.",
            home.skills_dir().display()
        );
        return Ok(());
    }
    let rows: Vec<Value> = catalog
        .skills
        .iter()
        .map(|s| {
            json!({
                "name": s.name,
                "status": s.status(),
                "bundle": s.bundle(),
                "source": s.source(),
                "description": s.description(),
                "problem": s.problem(),
            })
        })
        .collect();
    rt.show_columns(
        &json!(rows),
        &[
            "name",
            "status",
            "bundle",
            "source",
            "description",
            "problem",
        ],
    )
}

async fn show(rt: &Runtime, home: &Home, name: &str) -> Result<()> {
    let catalog = Catalog::load(home)?;
    let skill = catalog
        .skills
        .iter()
        .find(|s| s.name == name)
        .with_context(|| format!("there is no skill '{name}'; `gz skills list` shows them"))?;
    let walk = skills::files::walk(&skill.dir);
    let findings = skills::files::scan_files(&walk.files);
    let value = json!({
        "name": skill.name,
        "status": skill.status(),
        "problem": skill.problem(),
        "description": skill.description(),
        "folder": skill.dir.display().to_string(),
        "origin": skill.origin.to_string(),
        "fields": skill.manifest.as_ref().map(|m| &m.extra),
        "install": skill.locked,
        "files": walk.files.iter().map(|f| json!({"path": f.rel, "size": f.bytes.len()})).collect::<Vec<_>>(),
        "findings": findings.iter().map(finding_json).collect::<Vec<_>>(),
    });
    if rt.format != OutputFormat::Table {
        return rt.show(value).await;
    }
    println!("{} · {}", skill.name, skill.status());
    println!("  {}", skill.description());
    if let Some(problem) = skill.problem() {
        output::warn(&problem);
    }
    println!("  folder: {}", skill.dir.display());
    if let Some(lock) = &skill.locked {
        println!("  source: {} (bundle {})", lock.source, lock.bundle);
        if let Some(commit) = &lock.commit {
            println!("  commit: {commit}");
        }
        println!("  installed: {}", lock.installed_at);
    }
    if let Some(manifest) = &skill.manifest {
        for (key, value) in &manifest.extra {
            println!("  {key}: {value} (shown only; grants nothing)");
        }
    }
    println!("  files:");
    for file in &walk.files {
        println!("    {} ({})", file.rel, human_size(file.bytes.len() as u64));
    }
    for line in findings.iter().map(finding_line) {
        println!("  {line}");
    }
    Ok(())
}

fn finding_json(found: &FileFinding) -> Value {
    json!({
        "severity": found.finding.severity.label(),
        "rule": found.finding.rule,
        "path": found.path,
        "line": found.finding.line,
        "why": found.finding.why,
    })
}

fn finding_line(found: &FileFinding) -> String {
    format!(
        "{} {} line {}: {} [{}]",
        found.finding.severity.label(),
        found.path,
        found.finding.line,
        found.finding.why,
        found.finding.rule
    )
}

/// Builds the plan, shows it, and installs once the user (or the flags)
/// agree.
fn install_confirmed(home: &Home, request: &Request, consent: ConsentArgs) -> Result<()> {
    if matches!(request.source, Source::Git { .. }) {
        output::info(&format!("Fetching {}…", request.source));
    }
    let plan = install::prepare(home, request)?;
    for line in plan.preview() {
        println!("{line}");
    }
    match plan.consent(consent.yes, consent.accept_warnings) {
        Consent::Refuse(why) => bail!(why),
        Consent::Proceed => {}
        Consent::Ask => ask_to_install(&plan, consent)?,
    }
    let names = install::install(home, &plan)?;
    output::success(&format!(
        "Installed {} into {}",
        names.join(", "),
        home.skills_dir().display()
    ));
    Ok(())
}

fn ask_to_install(plan: &Plan, consent: ConsentArgs) -> Result<()> {
    if !std::io::stdin().is_terminal() {
        let flags = if matches!(plan.verdict(), install::Verdict::Warnings(_)) {
            "--yes --accept-warnings"
        } else {
            "--yes"
        };
        bail!("not installed: confirm with {flags} when not running interactively");
    }
    if consent.yes {
        output::warn("--yes doesn't cover warnings; add --accept-warnings, or answer here");
    }
    let answer = prompt_line(&format!("Install {}? [y/N] ", plan.names().join(", ")))?;
    if matches!(answer.to_ascii_lowercase().as_str(), "y" | "yes") {
        Ok(())
    } else {
        bail!("cancelled; nothing was copied")
    }
}

fn verify(rt: &Runtime, home: &Home) -> Result<()> {
    let catalog = Catalog::load(home)?;
    let rows: Vec<Value> = catalog
        .skills
        .iter()
        .map(|s| {
            let check = match (&s.health, &s.locked) {
                (Health::Ok, Some(_)) => "matches the lockfile",
                (Health::Ok, None) => "hand-made; scan clean",
                (Health::Tampered(_), _) => "changed since install",
                (Health::Blocked(_), _) => "scan found HIGH risks",
                (Health::Invalid(_), _) => "invalid SKILL.md",
            };
            json!({"name": s.name, "status": s.status(), "check": check, "problem": s.problem()})
        })
        .collect();
    let failing = catalog
        .skills
        .iter()
        .filter(|s| !matches!(s.health, Health::Ok))
        .count();
    rt.show_columns(&json!(rows), &["name", "status", "check", "problem"])?;
    if failing > 0 {
        bail!("{failing} skill(s) are disabled until fixed (see above)");
    }
    Ok(())
}

fn accept_changed(home: &Home, name: &str, yes: bool) -> Result<()> {
    install::accept_changes(home, name, &|findings| {
        for line in findings.iter().map(finding_line) {
            println!("{line}");
        }
        crate::cmd::confirm(yes, &format!("Trust {name} as it is now?"))
    })?;
    output::success(&format!("{name} is trusted again as it is now"));
    Ok(())
}

fn folders(rt: &Runtime, home: &Home, action: Option<&SkillFoldersAction>) -> Result<()> {
    match action {
        Some(SkillFoldersAction::Add { path }) => {
            let added = skills::add_folder(home, path)?;
            output::success(&format!("Added skill folder {}", added.display()));
        }
        Some(SkillFoldersAction::Remove { path }) => {
            let removed = skills::remove_folder(home, path)?;
            output::success(&format!(
                "Removed skill folder {} (its skills stay on disk)",
                removed.display()
            ));
        }
        None => {
            let mut rows = vec![
                json!({"order": "personal", "folder": home.skills_dir().display().to_string()}),
            ];
            rows.extend(home.settings()?.folders.iter().enumerate().map(|(i, f)| {
                json!({"order": (i + 1).to_string(), "folder": f.display().to_string(), "exists": f.is_dir()})
            }));
            rt.show_columns(&json!(rows), &["order", "folder", "exists"])?;
        }
    }
    Ok(())
}
