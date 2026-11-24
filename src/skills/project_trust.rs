//! Trust for project skills (`.gz/skills` in the open folder). They come
//! with whatever repository is open, so they get the team skills' bar: HIGH
//! scan findings always block, and a version with MEDIUM findings loads
//! only once the user trusted that exact version. Trust is kept per skill
//! folder, by content hash, in `project-skills-trust.toml` in the gz config
//! folder; any change to the skill asks again.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::assistant::private_file::write_private;
use crate::skills::files::{self, FileFinding};
use crate::skills::scan::Severity;
use crate::skills::team::{Diff, Review, ReviewKind, TrustEntry};
use crate::skills::{Catalog, Health, Home, Origin, Skill};

#[derive(Debug, Default, Serialize, Deserialize)]
struct TrustFile {
    /// Keyed by the skill folder's full path.
    #[serde(default)]
    skills: BTreeMap<String, TrustEntry>,
}

/// The project skill versions the user trusted.
#[derive(Debug, Default)]
pub struct ProjectTrust {
    path: PathBuf,
    entries: BTreeMap<String, TrustEntry>,
}

impl ProjectTrust {
    pub fn load(home: &Home) -> Result<Self> {
        let path = home.project_trust_path();
        let entries = if path.exists() {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("cannot read {}", path.display()))?;
            toml::from_str::<TrustFile>(&text)
                .with_context(|| format!("cannot parse {}; fix or delete it", path.display()))?
                .skills
        } else {
            BTreeMap::new()
        };
        Ok(Self { path, entries })
    }

    pub fn get(&self, dir: &Path) -> Option<&TrustEntry> {
        self.entries.get(&key(dir))
    }

    fn set(&mut self, dir: &Path, entry: TrustEntry) {
        self.entries.insert(key(dir), entry);
    }

    fn save(&self) -> Result<()> {
        let file = TrustFile {
            skills: self.entries.clone(),
        };
        let text = toml::to_string_pretty(&file).context("cannot encode project skill trust")?;
        write_private(&self.path, &text)
    }
}

fn key(dir: &Path) -> String {
    dir.canonicalize()
        .unwrap_or_else(|_| dir.to_path_buf())
        .display()
        .to_string()
}

/// A project skill's health at load: HIGH findings block, and MEDIUM ones
/// need this exact version trusted.
pub(crate) fn health(dir: &Path, trusted: Option<&TrustEntry>) -> Health {
    let walk = files::walk(dir);
    let (high, medium): (Vec<FileFinding>, Vec<FileFinding>) = files::scan_files(&walk.files)
        .into_iter()
        .partition(|f| f.finding.severity == Severity::High);
    if !high.is_empty() {
        return Health::Blocked(high);
    }
    let is_trusted = trusted.is_some_and(|t| t.sha256 == walk.tree_sha256());
    if medium.is_empty() || is_trusted {
        Health::Ok
    } else {
        Health::Untrusted(medium)
    }
}

/// The untrusted project skill `name` in `catalog`, if that is what it is.
pub(crate) fn untrusted<'a>(catalog: &'a Catalog, name: &str) -> Option<&'a Skill> {
    catalog.skills.iter().find(|s| {
        s.name == name
            && s.shadowed_by.is_none()
            && matches!(s.origin, Origin::Project(_))
            && matches!(s.health, Health::Untrusted(_))
    })
}

/// The review for trusting the project skill `skill` as it is now.
pub(crate) fn review(home: &Home, skill: &Skill) -> Result<Review> {
    let Health::Untrusted(findings) = &skill.health else {
        bail!(
            "{} needs no trust: it is clean or already trusted as it is",
            skill.name
        );
    };
    let walk = files::walk(&skill.dir);
    let now: BTreeMap<String, String> = walk
        .files
        .iter()
        .map(|f| (f.rel.clone(), f.sha256.clone()))
        .collect();
    let store = ProjectTrust::load(home)?;
    Ok(Review {
        kind: ReviewKind::Project,
        name: skill.name.clone(),
        key: skill.dir.display().to_string(),
        sha256: walk.tree_sha256(),
        updated_by: None,
        updated_at: None,
        findings: findings.clone(),
        diff: Diff::between(store.get(&skill.dir), &now),
    })
}

/// Trusts the version of the project skill at `dir` the user reviewed
/// (`sha256`); refuses if it changed since.
pub(crate) fn trust(home: &Home, dir: &Path, name: &str, sha256: &str) -> Result<()> {
    let walk = files::walk(dir);
    if let Some(problem) = walk.problems.first() {
        bail!("{name} can't be trusted: {problem}");
    }
    if walk.tree_sha256() != sha256 {
        bail!("{name} changed while you were reviewing it; review it again");
    }
    let mut store = ProjectTrust::load(home)?;
    store.set(
        dir,
        TrustEntry {
            sha256: sha256.to_string(),
            files: walk
                .files
                .iter()
                .map(|f| (f.rel.clone(), f.sha256.clone()))
                .collect(),
        },
    );
    store.save()
}
