//! Finding the skills in a fetched source. Four layouts are understood:
//! a single skill (`SKILL.md` at the root), a folder of skills
//! (`skills/*/SKILL.md` or `*/SKILL.md`), a Claude-Code-style plugin
//! (`.claude-plugin/plugin.json` with `skills/`), and a marketplace
//! (`.claude-plugin/marketplace.json`, or `plugins/*/skills/*`).
//!
//! A gz plugin is a bundle of skills, nothing more: hooks, MCP servers,
//! agents, commands and binaries a bundle carries are reported as skipped
//! and never copied.

use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};

use crate::skills::manifest::SKILL_FILE;
use crate::skills::source::{check_subdir, slug};

/// Why non-skill parts of a bundle are left behind.
pub const SKIPPED_REASON: &str =
    "gz plugins carry skills only; tools come from your Groundzero MCP connections";

const PLUGIN_DIR: &str = ".claude-plugin";
const PLUGIN_MANIFEST: &str = "plugin.json";
const MARKETPLACE_MANIFEST: &str = "marketplace.json";
/// Plugin parts gz never installs: (manifest key, files or folders, label).
const NON_SKILL_PARTS: &[(&str, &[&str], &str)] = &[
    ("hooks", &["hooks", "hooks.json"], "hooks"),
    ("mcpServers", &[".mcp.json"], "MCP servers"),
    ("agents", &["agents"], "agents"),
    ("commands", &["commands"], "commands"),
    ("lspServers", &[".lsp.json"], "LSP servers"),
    ("outputStyles", &["output-styles"], "output styles"),
    ("", &["bin"], "binaries"),
];

/// A skill folder found in the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundSkill {
    pub dir: PathBuf,
    /// Its folder relative to the source root (`.` for the root itself).
    pub rel: String,
    pub bundle: String,
    /// Whether the folder's own name must equal the skill's name (false
    /// for a skill at the root of a checkout, whose folder is gz's).
    pub named_by_folder: bool,
}

/// Something in the source that isn't installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub what: String,
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default)]
pub struct Layout {
    pub skills: Vec<FoundSkill>,
    pub skipped: Vec<Skipped>,
}

/// The skills under `top` (a checkout or local folder), where `root` is
/// `top` or a subfolder of it. `root_is_named` says whether a skill at
/// `root` itself must match its folder's name.
pub fn detect(top: &Path, root: &Path, bundle: &str, root_is_named: bool) -> Result<Layout> {
    let mut layout = Layout::default();
    let ctx = Ctx { top };
    if root.join(SKILL_FILE).is_file() {
        layout.skills.push(ctx.found(root, bundle, root_is_named));
        return Ok(layout);
    }
    let plugin_dir = root.join(PLUGIN_DIR);
    if plugin_dir.join(MARKETPLACE_MANIFEST).is_file() {
        ctx.marketplace(root, &mut layout)?;
    } else if plugin_dir.join(PLUGIN_MANIFEST).is_file() {
        ctx.plugin(root, bundle, &mut layout)?;
    } else if root.join("plugins").is_dir() && !ctx.plugin_dirs(&root.join("plugins")).is_empty() {
        for dir in ctx.plugin_dirs(&root.join("plugins")) {
            let name = dir_name(&dir);
            ctx.plugin(&dir, &slug(&name), &mut layout)?;
        }
    } else {
        let skills_dir = root.join("skills");
        let parent = if skills_dir.is_dir() {
            skills_dir
        } else {
            root.to_path_buf()
        };
        layout.skills.extend(ctx.skill_children(&parent, bundle));
        layout.skipped.extend(ctx.non_skill_parts(root, None));
    }
    if layout.skills.is_empty() {
        bail!(
            "no skills found in {}: expected SKILL.md, skills/*/SKILL.md, */SKILL.md, a .claude-plugin/plugin.json or a marketplace",
            ctx.rel(root)
        );
    }
    Ok(layout)
}

struct Ctx<'a> {
    top: &'a Path,
}

impl Ctx<'_> {
    fn rel(&self, path: &Path) -> String {
        let rel = path.strip_prefix(self.top).unwrap_or(path);
        let text = rel.to_string_lossy().replace('\\', "/");
        if text.is_empty() {
            ".".to_string()
        } else {
            text
        }
    }

    fn found(&self, dir: &Path, bundle: &str, named_by_folder: bool) -> FoundSkill {
        FoundSkill {
            dir: dir.to_path_buf(),
            rel: self.rel(dir),
            bundle: bundle.to_string(),
            named_by_folder,
        }
    }

    /// Real (non-symlink) subfolders of `parent` that hold a `SKILL.md`.
    fn skill_children(&self, parent: &Path, bundle: &str) -> Vec<FoundSkill> {
        real_subdirs(parent)
            .into_iter()
            .filter(|dir| dir.join(SKILL_FILE).is_file())
            .map(|dir| self.found(&dir, bundle, true))
            .collect()
    }

    fn plugin_dirs(&self, parent: &Path) -> Vec<PathBuf> {
        real_subdirs(parent)
            .into_iter()
            .filter(|dir| dir.join("skills").is_dir() || dir.join(PLUGIN_DIR).is_dir())
            .collect()
    }

    /// A Claude-Code-style plugin folder: its skills, and what's skipped.
    fn plugin(&self, dir: &Path, fallback: &str, layout: &mut Layout) -> Result<()> {
        let manifest_path = dir.join(PLUGIN_DIR).join(PLUGIN_MANIFEST);
        let manifest = if manifest_path.is_file() {
            read_json(&manifest_path)?
        } else {
            Value::Null
        };
        let bundle = manifest["name"]
            .as_str()
            .map(slug)
            .unwrap_or_else(|| fallback.to_string());
        let skill_paths: Vec<String> = match &manifest["skills"] {
            Value::String(path) => vec![path.clone()],
            Value::Array(paths) => paths
                .iter()
                .filter_map(|p| p.as_str().map(str::to_string))
                .collect(),
            _ => vec!["skills".to_string()],
        };
        for raw in skill_paths {
            let skills_dir = self.inside(dir, &raw)?;
            if skills_dir.join(SKILL_FILE).is_file() {
                layout.skills.push(self.found(&skills_dir, &bundle, true));
            } else {
                layout
                    .skills
                    .extend(self.skill_children(&skills_dir, &bundle));
            }
        }
        layout
            .skipped
            .extend(self.non_skill_parts(dir, Some(&manifest)));
        Ok(())
    }

    /// A marketplace: each plugin it lists that lives in this repository.
    fn marketplace(&self, root: &Path, layout: &mut Layout) -> Result<()> {
        let manifest = read_json(&root.join(PLUGIN_DIR).join(MARKETPLACE_MANIFEST))?;
        let plugins = manifest["plugins"]
            .as_array()
            .context("marketplace.json has no `plugins` list")?;
        for plugin in plugins {
            let name = plugin["name"].as_str().unwrap_or("plugin").to_string();
            match &plugin["source"] {
                Value::String(path) => {
                    let dir = self.inside(root, path)?;
                    self.plugin(&dir, &slug(&name), layout)?;
                }
                other => layout.skipped.push(Skipped {
                    what: format!("plugin {name}"),
                    path: other.to_string(),
                    reason: "it lives in another repository; install it from its own URL".into(),
                }),
            }
        }
        Ok(())
    }

    /// `raw`, a path from a manifest, resolved inside `base` — never above
    /// the checkout, and never through a symlink.
    fn inside(&self, base: &Path, raw: &str) -> Result<PathBuf> {
        let trimmed = raw.trim().trim_start_matches("./");
        if trimmed.is_empty() || trimmed == "." {
            return Ok(base.to_path_buf());
        }
        let rel = check_subdir(trimmed)
            .with_context(|| format!("the manifest path '{raw}' leaves the bundle"))?;
        let path = base.join(&rel);
        let mut walked = base.to_path_buf();
        for part in Path::new(&rel).components() {
            walked.push(part);
            let meta = std::fs::symlink_metadata(&walked)
                .with_context(|| format!("the manifest path '{raw}' doesn't exist"))?;
            if meta.file_type().is_symlink() {
                bail!("the manifest path '{raw}' goes through a symlink");
            }
        }
        let real = path.canonicalize()?;
        let top = self.top.canonicalize()?;
        if !real.starts_with(&top) {
            bail!("the manifest path '{raw}' leaves the bundle");
        }
        Ok(path)
    }

    /// Hooks, MCP servers, agents, commands and binaries in `dir`.
    fn non_skill_parts(&self, dir: &Path, manifest: Option<&Value>) -> Vec<Skipped> {
        let mut out = Vec::new();
        for (key, paths, label) in NON_SKILL_PARTS {
            let on_disk = paths.iter().map(|p| dir.join(p)).find(|p| p.exists());
            let declared = !key.is_empty() && manifest.is_some_and(|m| !m[*key].is_null());
            if on_disk.is_none() && !declared {
                continue;
            }
            let path = match on_disk {
                Some(path) => self.rel(&path),
                None => format!(
                    "{}/{PLUGIN_MANIFEST} ({key})",
                    self.rel(&dir.join(PLUGIN_DIR))
                ),
            };
            out.push(Skipped {
                what: label.to_string(),
                path,
                reason: SKIPPED_REASON.to_string(),
            });
        }
        out
    }
}

fn real_subdirs(parent: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(parent)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .map(|e| e.path())
                .filter(|p| !dir_name(p).starts_with('.'))
                .collect()
        })
        .unwrap_or_default();
    dirs.sort();
    dirs
}

pub fn dir_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default()
}

fn read_json(path: &Path) -> Result<Value> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("{} is not valid JSON", path.display()))
}
