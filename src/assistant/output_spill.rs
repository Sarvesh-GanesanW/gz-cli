//! Long command and script output, saved whole instead of thrown away.
//! A result too big for one tool result keeps its start and end for the
//! model, and the full text goes to a private file the model can page
//! through with `read_file` (offset and limit) or search with
//! `search_files`: the only files outside the open folders those tools
//! may open (see [`crate::assistant::sandbox::Sandbox::resolve_readable`]).
//!
//! The files live in one owner-only folder per session in the system's
//! temp folder, at most [`MAX_TOTAL_BYTES`] in all (the oldest go first),
//! and are deleted when the session ends. Folders left behind by a gz that
//! crashed are swept the next time one is made.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};

use crate::assistant::tools::{self, HeadTail, ToolOutput, MAX_OUTPUT_BYTES};

/// Most bytes the saved outputs of one session take; older ones are
/// deleted to make room.
pub const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
const DIR_PREFIX: &str = "gz-output-";
/// Room for the line, newlines included, that says where the text was cut.
const CUT_NOTE_BYTES: usize = 160;
/// A leftover folder this old is swept even when its process can't be
/// checked.
const STALE_AFTER: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// The session's saved outputs. Cheap to clone; clones share the folder,
/// which is deleted when the last clone goes.
#[derive(Debug, Clone)]
pub struct OutputSpill {
    store: Arc<Mutex<Store>>,
}

#[derive(Debug)]
struct Store {
    /// Where the session's folder is made.
    parent: PathBuf,
    /// Made on the first save.
    dir: Option<PathBuf>,
    /// Saved files, oldest first, with their sizes.
    files: VecDeque<(PathBuf, u64)>,
    total: u64,
    next: u64,
}

impl Default for OutputSpill {
    fn default() -> Self {
        Self::in_folder(&std::env::temp_dir())
    }
}

impl OutputSpill {
    /// Saved outputs in a new folder under `parent` (made on first use).
    pub fn in_folder(parent: &Path) -> Self {
        Self {
            store: Arc::new(Mutex::new(Store {
                parent: parent.to_path_buf(),
                dir: None,
                files: VecDeque::new(),
                total: 0,
                next: 0,
            })),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Store> {
        // Plain bookkeeping, still valid if a holder panicked.
        self.store.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Whether `path` (absolute, symlinks resolved) is an output this
    /// session saved and still keeps.
    pub fn contains(&self, path: &Path) -> bool {
        self.lock().files.iter().any(|(saved, _)| saved == path)
    }

    /// Saves `text` and returns its path.
    pub fn save(&self, text: &str) -> Result<PathBuf> {
        self.lock().save(text.as_bytes())
    }

    /// The tool result for `text`: whole when it fits one result,
    /// otherwise a first line naming the saved file that holds all of it,
    /// then the text's start and end. `lost_bytes` is how much output gz
    /// could not keep before saving (marked inside `text`), so the note
    /// never claims a file is whole when it is not.
    ///
    /// The file is named first so that notes appended later (hook
    /// feedback, a sandbox note) and the re-cut they may cause
    /// ([`ToolOutput::refit`] keeps the start) never lose it.
    pub fn fit(&self, text: String, is_error: bool, lost_bytes: usize) -> ToolOutput {
        if tools::fits_one_result(&text) {
            return plain(text, is_error);
        }
        let saved = match self.save(&text) {
            Ok(path) => path,
            Err(err) => {
                let mut out = plain(text, is_error);
                out.output
                    .push_str(&format!("\n[gz could not save the whole output: {err:#}]"));
                return out.refit();
            }
        };
        let what = match lost_bytes {
            0 => format!("The whole output ({} lines) is saved", text.lines().count()),
            lost => format!(
                "The output gz kept ({} lines; {lost} bytes it could not keep are marked in it) \
                 is saved",
                text.lines().count()
            ),
        };
        let saved_note = format!(
            "[{what} in {}: page through it with read_file (offset and limit) or search it \
             with search_files. Its middle is cut below.]",
            saved.display()
        );
        let room = MAX_OUTPUT_BYTES.saturating_sub(tools::json_size(&saved_note) + CUT_NOTE_BYTES);
        let Some(cut) = HeadTail::of(&text, room) else {
            return plain(text, is_error);
        };
        let cut_note = format!(
            "… {} characters ({} bytes) cut here by gz; they are in the saved file …",
            cut.cut_chars, cut.cut_bytes
        );
        ToolOutput {
            output: format!("{saved_note}\n{}", cut.join(&cut_note)),
            is_error,
            truncated: true,
        }
    }
}

fn plain(text: String, is_error: bool) -> ToolOutput {
    match is_error {
        true => ToolOutput::error(text),
        false => ToolOutput::ok(text),
    }
}

impl Store {
    fn save(&mut self, bytes: &[u8]) -> Result<PathBuf> {
        let size = bytes.len() as u64;
        self.make_room(size);
        let dir = self.dir()?;
        self.next += 1;
        let path = dir.join(format!("output-{}.txt", self.next));
        write_new_private(&path, bytes)
            .with_context(|| format!("cannot save {}", path.display()))?;
        self.files.push_back((path.clone(), size));
        self.total += size;
        Ok(path)
    }

    /// Deletes the oldest outputs until `size` more bytes fit.
    fn make_room(&mut self, size: u64) {
        while self.total + size > MAX_TOTAL_BYTES {
            let Some((oldest, old_size)) = self.files.pop_front() else {
                break;
            };
            // Gone from the list either way: the model may no longer open it.
            std::fs::remove_file(&oldest).ok();
            self.total -= old_size;
        }
    }

    fn dir(&mut self) -> Result<PathBuf> {
        if let Some(dir) = &self.dir {
            return Ok(dir.clone());
        }
        sweep_leftovers(&self.parent);
        let name = format!("{DIR_PREFIX}{}-{}", std::process::id(), random_suffix());
        let dir = self.parent.join(name);
        create_private_dir(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
        let dir = dir.canonicalize()?;
        self.dir = Some(dir.clone());
        Ok(dir)
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        if let Some(dir) = &self.dir {
            std::fs::remove_dir_all(dir).ok();
        }
    }
}

fn random_suffix() -> String {
    use rand::Rng;
    format!("{:016x}", rand::thread_rng().gen::<u64>())
}

/// A new folder only the user can open; fails if the name is taken, so a
/// folder someone else planted is never used.
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

/// A new file only the user can read.
fn write_new_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(bytes)
}

/// Deletes saved-output folders whose gz is gone. Best effort: one that
/// can't be read or removed stays.
fn sweep_leftovers(parent: &Path) {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(rest) = name.strip_prefix(DIR_PREFIX) else {
            continue;
        };
        let pid = rest.split('-').next().and_then(|p| p.parse::<u32>().ok());
        let age = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| SystemTime::now().duration_since(t).ok());
        let is_stale = age.is_some_and(|age| age > STALE_AFTER);
        let is_orphan = pid.is_some_and(|pid| pid != std::process::id() && !is_running(pid));
        if is_stale || is_orphan {
            std::fs::remove_dir_all(entry.path()).ok();
        }
    }
}

#[cfg(unix)]
fn is_running(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // Signal 0 checks the process without touching it; EPERM means it
    // exists but belongs to someone else.
    let alive = unsafe { libc::kill(pid, 0) } == 0;
    alive || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn is_running(_pid: u32) -> bool {
    // Without a cheap check, only the age sweep applies.
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assistant::sandbox::tests::temp_dir;

    #[test]
    fn long_output_is_saved_whole_and_named_in_the_result() {
        let parent = temp_dir("spill-fit");
        let spill = OutputSpill::in_folder(&parent);
        let text: String = (1..=20_000).map(|i| format!("line {i}\n")).collect();
        let out = spill.fit(text.clone(), true, 0);
        assert!(out.truncated && out.is_error);
        assert!(tools::json_size(&out.output) <= MAX_OUTPUT_BYTES);
        assert!(out
            .output
            .starts_with("[The whole output (20000 lines) is saved in "));
        assert!(out.output.contains("]\nline 1\n"));
        assert!(out.output.ends_with("line 20000\n"));
        let path = out
            .output
            .split("is saved in ")
            .nth(1)
            .and_then(|rest| rest.split(':').next())
            .map(PathBuf::from)
            .unwrap();
        assert!(spill.contains(&path), "{}", out.output);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        assert!(out.output.contains("20000 lines"));
        assert!(out.output.contains("read_file (offset and limit)"));

        let short = spill.fit("ok\n".into(), false, 0);
        assert_eq!((short.output.as_str(), short.truncated), ("ok\n", false));
    }

    #[test]
    fn notes_added_later_never_cut_the_saved_files_name() {
        let spill = OutputSpill::in_folder(&temp_dir("spill-refit"));
        let text: String = (0..30_000).map(|i| format!("row {i}\n")).collect();
        let out = spill.fit(text, false, 0);
        let saved = out.output.lines().next().unwrap().to_string();
        assert!(saved.contains(" is saved in "), "{saved}");
        for feedback in [100, 800, 2_000, 40_000] {
            let mut later = out.clone();
            later
                .output
                .push_str(&"formatter said: fixed\n".repeat(feedback / 22));
            let later = later.refit();
            assert!(tools::json_size(&later.output) <= MAX_OUTPUT_BYTES);
            assert!(
                later.output.starts_with(&saved),
                "{feedback} bytes of notes cut the name"
            );
        }
    }

    #[test]
    fn output_gz_could_not_keep_is_not_called_whole() {
        let spill = OutputSpill::in_folder(&temp_dir("spill-lost"));
        let text: String = (0..30_000).map(|i| format!("row {i}\n")).collect();
        let out = spill.fit(text, false, 4096);
        let first = out.output.lines().next().unwrap();
        assert!(!first.contains("whole"), "{first}");
        assert!(first.contains("4096 bytes it could not keep"), "{first}");
    }

    #[cfg(unix)]
    #[test]
    fn saved_outputs_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let spill = OutputSpill::in_folder(&temp_dir("spill-mode"));
        let path = spill.save("secret build log").unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
    }

    #[test]
    fn the_oldest_outputs_go_when_the_total_is_reached() {
        let spill = OutputSpill::in_folder(&temp_dir("spill-bound"));
        let chunk = "x".repeat((MAX_TOTAL_BYTES / 3) as usize + 1);
        let first = spill.save(&chunk).unwrap();
        let second = spill.save(&chunk).unwrap();
        let third = spill.save(&chunk).unwrap();
        assert!(!spill.contains(&first) && !first.exists());
        assert!(spill.contains(&second) && spill.contains(&third));
        assert!(spill.lock().total <= MAX_TOTAL_BYTES);
    }

    #[test]
    fn the_folder_goes_with_the_session() {
        let spill = OutputSpill::in_folder(&temp_dir("spill-drop"));
        let path = spill.save("x").unwrap();
        let dir = path.parent().unwrap().to_path_buf();
        let clone = spill.clone();
        drop(spill);
        assert!(dir.exists(), "a clone still holds it");
        drop(clone);
        assert!(!dir.exists());
    }

    #[cfg(unix)]
    #[test]
    fn folders_left_by_a_gz_that_is_gone_are_swept() {
        let parent = temp_dir("spill-sweep");
        // Above every system's largest pid, so never a running process.
        let dead = parent.join(format!("{DIR_PREFIX}999999999-abc"));
        let mine = parent.join(format!("{DIR_PREFIX}{}-abc", std::process::id()));
        let other = parent.join("unrelated");
        for dir in [&dead, &mine, &other] {
            std::fs::create_dir(dir).unwrap();
        }
        OutputSpill::in_folder(&parent).save("x").unwrap();
        assert!(!dead.exists());
        assert!(mine.exists() && other.exists());
    }
}
