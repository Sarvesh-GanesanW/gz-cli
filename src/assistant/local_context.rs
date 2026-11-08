//! What gz tells the chat about this machine beyond the tool catalog, once
//! the server advertises `features.localContext`: the `clientContext`
//! request field (current folder, open folders, instruction files,
//! memories) and the two client tools that change it, `change_directory`
//! and `memory`. Older servers reject unknown tool names, so nothing here
//! is sent unless the capability is there.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::Result;
use serde::Serialize;
use serde_json::Value;

use crate::assistant::instructions::{self, Instruction, Source};
use crate::assistant::memory::{self, Memory, MemoryStore};
use crate::assistant::sandbox::{Sandbox, MAX_ROOTS};

/// Client tools that only exist with `features.localContext`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextTool {
    ChangeDirectory,
    Memory,
}

impl ContextTool {
    pub const ALL: [ContextTool; 2] = [ContextTool::ChangeDirectory, ContextTool::Memory];

    pub fn as_str(self) -> &'static str {
        match self {
            ContextTool::ChangeDirectory => "change_directory",
            ContextTool::Memory => "memory",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.as_str() == name)
    }

    /// The tool behind the server's display name ("Change directory").
    pub fn from_display(name: &str) -> Option<Self> {
        Self::parse(&name.trim().to_ascii_lowercase().replace([' ', '-'], "_"))
    }
}

/// What the server accepts in `clientContext` (`localContextLimits`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub instruction_chars: usize,
    pub instruction_files: usize,
    pub memory_entries: usize,
    pub memory_chars: usize,
    pub memory_entry_chars: usize,
    pub skills: usize,
    pub skill_description_chars: usize,
}

impl Default for Limits {
    /// The contract's values, used for any limit the server leaves out.
    fn default() -> Self {
        Self {
            instruction_chars: 20_000,
            instruction_files: 8,
            memory_entries: 200,
            memory_chars: 20_000,
            memory_entry_chars: memory::MAX_ENTRY_CHARS,
            skills: 200,
            skill_description_chars: 1024,
        }
    }
}

/// The limits when the server advertises `features.localContext`, else
/// `None` (send no `clientContext` and none of its tools).
pub fn limits_from(capabilities: &Value) -> Option<Limits> {
    if capabilities["features"]["localContext"].as_bool() != Some(true) {
        return None;
    }
    let advertised = &capabilities["localContextLimits"];
    let read = |key: &str, default: usize| advertised[key].as_u64().map_or(default, |n| n as usize);
    let d = Limits::default();
    Some(Limits {
        instruction_chars: read("instructionChars", d.instruction_chars),
        instruction_files: read("instructionFiles", d.instruction_files),
        memory_entries: read("memoryEntries", d.memory_entries),
        memory_chars: read("memoryChars", d.memory_chars),
        // Never above what gz itself lets a memory be.
        memory_entry_chars: read("memoryEntryChars", d.memory_entry_chars)
            .min(memory::MAX_ENTRY_CHARS),
        skills: read("skills", d.skills),
        skill_description_chars: read("skillDescriptionChars", d.skill_description_chars),
    })
}

/// Just the server's capabilities, for runs that don't load the model
/// list (one-shot prompts): `None` when they can't be read.
pub async fn probe(http: &crate::http::ApiClient) -> Option<Limits> {
    let capabilities = http
        .request_json(
            "chat",
            reqwest::Method::GET,
            "/chat/capabilities",
            &[],
            None,
        )
        .await
        .ok()?;
    limits_from(&capabilities)
}

/// The `clientContext` request field. Empty parts are left out.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ClientContext {
    #[serde(skip_serializing_if = "String::is_empty")]
    pub cwd: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub roots: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub instructions: Vec<InstructionFile>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub memories: Vec<MemoryEntry>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<SkillSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InstructionFile {
    pub source: Source,
    pub path: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MemoryEntry {
    pub id: String,
    pub text: String,
}

/// An installed skill as the chat lists it (filled by the skills module).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkillSummary {
    pub name: String,
    pub description: String,
}

impl ClientContext {
    /// The context for the sandbox's current folder.
    pub fn build(
        sandbox: &Sandbox,
        instructions: &[Instruction],
        memories: &[Memory],
        limits: &Limits,
    ) -> Self {
        Self {
            cwd: Some(sandbox.display_folder(&sandbox.cwd()))
                .filter(|p| is_sendable_path(p))
                .unwrap_or_default(),
            roots: sandbox
                .roots()
                .iter()
                .map(|root| sandbox.display_folder(root))
                .filter(|p| is_sendable_path(p))
                .take(MAX_ROOTS)
                .collect(),
            instructions: instructions
                .iter()
                .filter(|i| is_sendable_path(&i.shown) && !i.text.trim().is_empty())
                .map(|i| InstructionFile {
                    source: i.source,
                    path: i.shown.clone(),
                    text: i.text.clone(),
                })
                .collect(),
            memories: sendable_memories(memories, limits),
            skills: Vec::new(),
        }
    }
}

/// Longest path the server takes in `clientContext`.
const MAX_PATH_CHARS: usize = 512;
/// Longest memory id the server takes.
const MAX_ID_CHARS: usize = 64;

/// Whether the server takes `path`: short, no control characters, no `..`
/// segment (gz sends canonical paths, so this only drops odd names).
fn is_sendable_path(path: &str) -> bool {
    !path.is_empty()
        && path.chars().count() <= MAX_PATH_CHARS
        && !path.chars().any(char::is_control)
        && !path.split(['/', '\\']).any(|segment| segment == "..")
}

/// Ids as the server takes them: `[A-Za-z0-9_-]{1,64}`.
fn is_sendable_id(id: &str) -> bool {
    (1..=MAX_ID_CHARS).contains(&id.len())
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Memories within the limits, oldest first. The file can be edited by
/// hand, so each entry is checked again rather than trusted.
fn sendable_memories(memories: &[Memory], limits: &Limits) -> Vec<MemoryEntry> {
    let mut total = 0;
    let mut out = Vec::new();
    for memory in memories {
        let chars = memory.text.chars().count();
        if out.len() >= limits.memory_entries || total + chars > limits.memory_chars {
            break;
        }
        let duplicate = out.iter().any(|m: &MemoryEntry| m.id == memory.id);
        if chars > limits.memory_entry_chars
            || memory::check_text(&memory.text).is_err()
            || !is_sendable_id(&memory.id)
            || duplicate
        {
            continue;
        }
        total += chars;
        out.push(MemoryEntry {
            id: memory.id.clone(),
            text: memory.text.clone(),
        });
    }
    out
}

/// Saves an "always allow this folder" answer; the default writes the
/// profile's `code_allowed_dirs`.
pub type SaveFolder = Arc<dyn Fn(&Path) -> Result<()> + Send + Sync>;

/// Folders outside the open ones that `change_directory` may enter
/// without asking.
#[derive(Clone)]
pub struct AllowedFolders {
    dirs: Arc<Mutex<Vec<PathBuf>>>,
    save: SaveFolder,
}

impl AllowedFolders {
    pub fn new(dirs: Vec<PathBuf>, save: SaveFolder) -> Self {
        Self {
            dirs: Arc::new(Mutex::new(dirs)),
            save,
        }
    }

    /// The profile's saved folders, saved back to the profile.
    pub fn for_profile(profile: &str, dirs: Vec<PathBuf>) -> Self {
        let profile = profile.to_string();
        let save: SaveFolder = Arc::new(move |folder: &Path| {
            let mut config = crate::config::load_config()?;
            let dirs = &mut config.profile_mut(&profile).code_allowed_dirs;
            if !dirs.iter().any(|d| d == folder) {
                dirs.push(folder.to_path_buf());
            }
            crate::config::save_config(&config).map(|_| ())
        });
        Self::new(dirs, save)
    }

    fn dirs(&self) -> MutexGuard<'_, Vec<PathBuf>> {
        self.dirs.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Whether `folder` is, or is inside, an always-allowed folder.
    pub fn allows(&self, folder: &Path) -> bool {
        self.dirs().iter().any(|dir| folder.starts_with(dir))
    }

    /// Allows `folder` from now on, and saves it for later sessions.
    pub fn remember(&self, folder: &Path) -> Result<()> {
        self.dirs().push(folder.to_path_buf());
        (self.save)(folder)
    }
}

/// The local-context settings a session and its background subagents
/// share: what the server takes, whether memory is on, and where the
/// user's files are.
#[derive(Clone)]
pub struct LocalContext {
    settings: Arc<Mutex<Settings>>,
    /// `None` when the config folder can't be found (no memory then).
    pub memory: Option<MemoryStore>,
    pub user_instructions: Option<PathBuf>,
    pub allowed: AllowedFolders,
}

#[derive(Debug, Clone, Copy)]
struct Settings {
    limits: Option<Limits>,
    memory_on: bool,
}

impl LocalContext {
    pub fn new(
        memory: Option<MemoryStore>,
        user_instructions: Option<PathBuf>,
        allowed: AllowedFolders,
    ) -> Self {
        Self {
            settings: Arc::new(Mutex::new(Settings {
                limits: None,
                memory_on: true,
            })),
            memory,
            user_instructions,
            allowed,
        }
    }

    /// The local context for `profile`: its saved folders and memory
    /// setting, and the files next to the gz config.
    pub fn for_profile(profile_name: &str, profile: &crate::config::Profile) -> Self {
        let context = Self::new(
            MemoryStore::open(profile_name).ok(),
            instructions::user_file().ok(),
            AllowedFolders::for_profile(profile_name, profile.code_allowed_dirs.clone()),
        );
        context.set_memory(profile.code_memory != Some(false));
        context
    }

    fn settings(&self) -> MutexGuard<'_, Settings> {
        self.settings.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The server's limits; `None` when it doesn't take local context.
    pub fn limits(&self) -> Option<Limits> {
        self.settings().limits
    }

    pub fn set_limits(&self, limits: Option<Limits>) {
        self.settings().limits = limits;
    }

    /// Whether memory is on (`/memory on|off`) and can be stored.
    pub fn memory_on(&self) -> bool {
        self.memory.is_some() && self.settings().memory_on
    }

    pub fn set_memory(&self, on: bool) {
        self.settings().memory_on = on;
    }

    /// The memory store, when memory is on.
    pub fn active_memory(&self) -> Option<&MemoryStore> {
        self.memory.as_ref().filter(|_| self.memory_on())
    }

    /// The extra `clientTools` names the server takes.
    pub fn tool_names(&self) -> Vec<&'static str> {
        if self.limits().is_none() {
            return Vec::new();
        }
        let mut names = vec![ContextTool::ChangeDirectory.as_str()];
        if self.memory_on() {
            names.push(ContextTool::Memory.as_str());
        }
        names
    }

    /// Instruction files for the sandbox's current folder.
    pub fn instructions(&self, sandbox: &Sandbox, limits: &Limits) -> Vec<Instruction> {
        instructions::discover(sandbox, self.user_instructions.as_deref(), limits)
    }

    /// The `clientContext` for the next run; `None` when the server
    /// doesn't take it. Files are read now, so edits apply right away.
    pub fn request_field(&self, sandbox: &Sandbox) -> Option<Value> {
        let limits = self.limits()?;
        let memories = self
            .active_memory()
            .and_then(|store| store.list().ok())
            .unwrap_or_default();
        let context = ClientContext::build(
            sandbox,
            &self.instructions(sandbox, &limits),
            &memories,
            &limits,
        );
        serde_json::to_value(context).ok()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    /// Allowed folders whose "always" answers land in `saved`.
    pub fn recording_folders(dirs: Vec<PathBuf>) -> (AllowedFolders, Arc<Mutex<Vec<PathBuf>>>) {
        let saved = Arc::new(Mutex::new(Vec::new()));
        let sink = saved.clone();
        let save: SaveFolder = Arc::new(move |folder: &Path| {
            sink.lock().unwrap().push(folder.to_path_buf());
            Ok(())
        });
        (AllowedFolders::new(dirs, save), saved)
    }

    #[test]
    fn local_context_is_only_on_when_advertised() {
        assert_eq!(limits_from(&json!({})), None);
        assert_eq!(
            limits_from(&json!({"features": {"localContext": false}})),
            None
        );
        let limits = limits_from(&json!({
            "features": {"localContext": true},
            "localContextLimits": {"instructionFiles": 3, "memoryEntryChars": 9000},
        }))
        .unwrap();
        assert_eq!(limits.instruction_files, 3);
        assert_eq!(
            limits.instruction_chars, 20_000,
            "missing limits use the contract's"
        );
        assert_eq!(limits.memory_entry_chars, 500, "never above gz's own cap");
    }

    #[test]
    fn memories_past_the_limits_or_hand_edited_badly_stay_home() {
        let memory = |id: &str, text: &str| Memory {
            id: id.into(),
            text: text.into(),
            created_at: 0,
            updated_at: 0,
        };
        let memories = [
            memory("m_1", "Uses tabs"),
            memory("m_2", "bad\u{202E}one"),
            memory("m_3", "Likes tea"),
            memory("m_1", "Same id twice"),
            memory("bad id!", "Odd id"),
            memory("m_4", "One too many"),
        ];
        let limits = Limits {
            memory_entries: 2,
            ..Limits::default()
        };
        let sent: Vec<String> = sendable_memories(&memories, &limits)
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(sent, ["m_1", "m_3"]);
    }

    #[test]
    fn paths_the_server_would_refuse_are_left_out() {
        assert!(is_sendable_path("~/code/gz-cli"));
        assert!(!is_sendable_path("~/code/../x"));
        assert!(!is_sendable_path("~/odd\nname"));
        assert!(!is_sendable_path(&format!("/{}", "a".repeat(600))));
        assert!(!is_sendable_path(""));
    }

    #[test]
    fn the_request_field_names_folders_from_home_and_leaves_out_empty_parts() {
        let home = crate::assistant::sandbox::tests::temp_dir("context-field");
        std::fs::create_dir_all(home.join("code/app")).unwrap();
        let sandbox = Sandbox::with_home(&home.join("code/app"), Some(&home)).unwrap();
        let (allowed, _) = recording_folders(Vec::new());
        let context = LocalContext::new(None, None, allowed);
        assert!(context.request_field(&sandbox).is_none(), "not advertised");
        assert!(context.tool_names().is_empty());
        context.set_limits(Some(Limits::default()));
        assert_eq!(
            context.request_field(&sandbox).unwrap(),
            json!({"cwd": "~/code/app", "roots": ["~/code/app"]})
        );
        assert_eq!(
            context.tool_names(),
            ["change_directory"],
            "no memory tool without a memory store"
        );
    }
}
