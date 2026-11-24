//! What the model has read in this conversation, so an edit or overwrite
//! only lands on a file whose current content the model has seen. A file
//! nobody read, or one that changed on disk after the read (the user, a
//! formatter, a background command), is refused with a message telling
//! the model to read it again. Writes the model makes count as reads of
//! the new content.
//!
//! The ledger lives in memory only: a new or resumed conversation starts
//! empty, so files are read afresh before they are changed. Each reader
//! (the main conversation, or a subagent the chat server runs inside it)
//! has its own record: a subagent edits only what it read itself.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{bail, Result};
use sha2::{Digest, Sha256};

/// The SHA-256 of a file's bytes at one moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fingerprint([u8; 32]);

impl Fingerprint {
    pub fn of(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }
}

/// Fingerprints by absolute path, for one reader.
type Seen = HashMap<PathBuf, Fingerprint>;

/// The files one reader read or wrote. Cheap to clone; clones share the
/// record. [`ReadLedger::for_reader`] gives another reader's view of the
/// same conversation.
#[derive(Debug, Clone, Default)]
pub struct ReadLedger {
    /// Every reader's record; `None` is the main conversation.
    readers: Arc<Mutex<HashMap<Option<String>, Seen>>>,
    reader: Option<String>,
}

impl ReadLedger {
    fn lock(&self) -> MutexGuard<'_, HashMap<Option<String>, Seen>> {
        // Plain data, still valid if a holder panicked.
        self.readers.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The same conversation's ledger as `reader` (a subagent's name, or
    /// `None` for the main conversation) sees it.
    pub fn for_reader(&self, reader: Option<&str>) -> Self {
        Self {
            readers: self.readers.clone(),
            reader: reader.map(str::to_string),
        }
    }

    /// Notes that this reader now knows `path` as `bytes` (a read, even of
    /// a few lines, or its own write).
    pub fn record(&self, path: &Path, bytes: &[u8]) {
        self.lock()
            .entry(self.reader.clone())
            .or_default()
            .insert(path.to_path_buf(), Fingerprint::of(bytes));
    }

    /// Forgets every reader's reads, for a new or resumed conversation.
    pub fn clear(&self) {
        self.lock().clear();
    }

    /// Whether this reader may change `path`, whose content is `current`
    /// now; `shown` names it in the refusal.
    pub fn check_known(&self, path: &Path, current: &[u8], shown: &str) -> Result<()> {
        let seen = self
            .lock()
            .get(&self.reader)
            .and_then(|seen| seen.get(path).copied());
        match seen {
            None => bail!(
                "{shown} has not been read in this conversation; read it with read_file \
                 first, then make the change"
            ),
            Some(seen) if seen != Fingerprint::of(current) => bail!(
                "{shown} changed on disk since you last read it (by the user, a formatter or a \
                 command); read it again with read_file before editing, then redo the change \
                 against what it holds now"
            ),
            Some(_) => Ok(()),
        }
    }
}

/// Refuses a write when the file is no longer what the change was worked
/// out against (`base`; `None` when the file did not exist), so a change
/// approved earlier never overwrites something newer.
pub fn check_unchanged(path: &Path, base: Option<Fingerprint>, shown: &str) -> Result<()> {
    let now = match std::fs::read(path) {
        Ok(bytes) => Some(Fingerprint::of(&bytes)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => bail!("cannot read {shown} before writing it ({err})"),
    };
    if now == base {
        return Ok(());
    }
    match base {
        None => bail!(
            "{shown} was created by something else after this change was planned, so gz did \
             not write it; read it with read_file, then decide whether to change it"
        ),
        Some(_) => bail!(
            "{shown} changed on disk after this change was planned (by the user, a formatter, \
             a hook or a command), so gz did not write it; read it again with read_file and \
             redo the change"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unread_and_changed_files_are_refused_with_a_read_hint() {
        let ledger = ReadLedger::default();
        let path = Path::new("/w/a.rs");
        let unread = ledger.check_known(path, b"one", "a.rs").unwrap_err();
        assert!(
            unread.to_string().contains("read it with read_file first"),
            "{unread}"
        );

        ledger.record(path, b"one");
        assert!(ledger.check_known(path, b"one", "a.rs").is_ok());
        let changed = ledger.check_known(path, b"two", "a.rs").unwrap_err();
        assert!(
            changed
                .to_string()
                .contains("changed on disk since you last read it"),
            "{changed}"
        );

        ledger.clear();
        assert!(ledger.check_known(path, b"one", "a.rs").is_err());
    }

    #[test]
    fn each_reader_edits_only_what_it_read_itself() {
        let main = ReadLedger::default();
        let helper = main.for_reader(Some("explorer"));
        let path = Path::new("/w/a.rs");
        main.record(path, b"one");
        assert!(helper.check_known(path, b"one", "a.rs").is_err());

        helper.record(path, b"one");
        assert!(helper.check_known(path, b"one", "a.rs").is_ok());
        assert!(main
            .for_reader(None)
            .check_known(path, b"one", "a.rs")
            .is_ok());

        main.clear();
        assert!(helper.check_known(path, b"one", "a.rs").is_err());
    }

    #[test]
    fn a_write_is_refused_when_the_file_moved_on_since_planning() {
        let (dir, _sandbox) = crate::assistant::sandbox::tests::temp_workspace("ledger-base");
        let path = dir.join("a.txt");
        assert!(check_unchanged(&path, None, "a.txt").is_ok());

        std::fs::write(&path, "planned against").unwrap();
        let base = Some(Fingerprint::of(b"planned against"));
        assert!(check_unchanged(&path, base, "a.txt").is_ok());
        let created = check_unchanged(&path, None, "a.txt").unwrap_err();
        assert!(created.to_string().contains("created by something else"));

        std::fs::write(&path, "someone else's text").unwrap();
        let err = check_unchanged(&path, base, "a.txt").unwrap_err();
        assert!(err.to_string().contains("changed on disk"), "{err}");
    }
}
