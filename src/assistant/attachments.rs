//! Files the user attaches to a turn: `@path` mentions and pasted images.
//!
//! Text files and folders are read locally (an explicit mention is the
//! user's consent) and sent inline with the prompt; images and documents
//! the chat accepts (pdf, spreadsheets) are uploaded through chat's own
//! document flow and referenced by id, exactly as the web app attaches them.

use anyhow::{bail, Context, Result};
use ignore::WalkBuilder;
use reqwest::Method;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use crate::assistant::sandbox::Sandbox;
use crate::http::ApiClient;

const CHAT: &str = "chat";
/// Lines kept from one mentioned file.
const MAX_FILE_LINES: usize = 2_000;
/// Bytes of inline file and folder context per turn.
const MAX_CONTEXT_BYTES: usize = 200_000;
/// Entries shown for a mentioned folder.
const MAX_LISTING: usize = 200;
/// The chat's own upload cap (gz-chat-eks `CHAT_MAX_UPLOAD_BYTES`).
pub const MAX_UPLOAD_BYTES: u64 = 50 * 1024 * 1024;
/// Bytes sniffed to tell text from binary.
const SNIFF_BYTES: usize = 8_192;

/// Image types the chat accepts as attachments (gz-chat-eks constants.py).
pub const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "webp", "gif"];
/// Binary documents the chat indexes; sent as uploads, never inlined.
const DOCUMENT_EXTENSIONS: &[&str] = &["pdf", "xlsx", "xls"];

/// What a mentioned path is, deciding how it travels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Directory,
    Text,
    Image,
    Document,
    /// Binary the chat can't take (zip, docx, executables).
    Unsupported,
}

fn extension(path: &Path) -> String {
    path.extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

pub fn is_image(path: &Path) -> bool {
    IMAGE_EXTENSIONS.contains(&extension(path).as_str())
}

/// Classifies an existing path: folders, then known upload types by
/// extension, then text vs binary by sniffing for NUL bytes.
pub fn classify(path: &Path) -> Result<Kind> {
    if path.is_dir() {
        return Ok(Kind::Directory);
    }
    let ext = extension(path);
    if IMAGE_EXTENSIONS.contains(&ext.as_str()) {
        return Ok(Kind::Image);
    }
    if DOCUMENT_EXTENSIONS.contains(&ext.as_str()) {
        return Ok(Kind::Document);
    }
    let mut head = vec![0u8; SNIFF_BYTES];
    let read = {
        use std::io::Read;
        let mut file =
            std::fs::File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
        file.read(&mut head)
            .with_context(|| format!("cannot read {}", path.display()))?
    };
    Ok(if head[..read].contains(&0) {
        Kind::Unsupported
    } else {
        Kind::Text
    })
}

/// `@path` and `@"path with spaces"` mentions, in order, without repeats.
/// An `@` only starts a mention at the beginning or after whitespace, so
/// emails and decorators (`me@site.com`, `x@y`) are left alone.
pub fn parse_mentions(text: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let starts = chars[i] == '@' && (i == 0 || chars[i - 1].is_whitespace());
        if !starts {
            i += 1;
            continue;
        }
        let (path, end) = if chars.get(i + 1) == Some(&'"') {
            let close = chars[i + 2..]
                .iter()
                .position(|c| *c == '"')
                .map(|p| i + 2 + p);
            match close {
                Some(close) => (chars[i + 2..close].iter().collect::<String>(), close + 1),
                None => (String::new(), chars.len()),
            }
        } else {
            let end = chars[i + 1..]
                .iter()
                .position(|c| c.is_whitespace())
                .map_or(chars.len(), |p| i + 1 + p);
            let raw: String = chars[i + 1..end].iter().collect();
            (
                raw.trim_end_matches([',', ';', ':', '!', '?', ')'])
                    .to_string(),
                end,
            )
        };
        if !path.is_empty() && !found.contains(&path) {
            found.push(path);
        }
        i = end;
    }
    found
}

/// The chip text for the `n`-th pasted image (1-based), as shown in the input.
pub fn image_chip(n: usize) -> String {
    format!("[Image #{n}]")
}

/// A turn ready to send: the prompt the model gets, what to show under
/// the user's message, what was read (for the audit log), and what to upload.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PreparedTurn {
    /// What the user typed, shown in the transcript.
    pub display: String,
    /// `display` plus inline `<file>`/`<directory>` blocks.
    pub prompt: String,
    /// Context lines like `Read src/math.rs (7 lines)`.
    pub notes: Vec<String>,
    /// Workspace-relative paths read for the prompt.
    pub reads: Vec<String>,
    /// Images and documents to attach by upload.
    pub uploads: Vec<PathBuf>,
}

impl PreparedTurn {
    pub fn plain(text: &str) -> Self {
        Self {
            display: text.to_string(),
            prompt: text.to_string(),
            ..Self::default()
        }
    }
}

/// Resolves every mention and pasted image in `text`. Fails (nothing is
/// sent) on a path that doesn't exist, is outside the folder, is a secret,
/// or is a binary the chat can't take.
pub fn prepare(sandbox: &Sandbox, text: &str, images: &[PathBuf]) -> Result<PreparedTurn> {
    let mut turn = PreparedTurn::plain(text);
    let mut blocks = String::new();
    for mention in parse_mentions(text) {
        let path = sandbox
            .resolve(&mention)
            .with_context(|| format!("can't attach @{mention}"))?;
        if !path.exists() {
            bail!("@{mention} doesn't exist in this folder");
        }
        let shown = sandbox.display(&path);
        match classify(&path)? {
            Kind::Directory => {
                let (block, count) = listing(sandbox, &path)?;
                if blocks.len() + block.len() > MAX_CONTEXT_BYTES {
                    turn.notes.push(format!(
                        "Skipped {shown}/: the turn's context limit was reached"
                    ));
                } else {
                    blocks.push_str(&block);
                    turn.notes.push(format!("Listed {shown}/ ({count} entries)"));
                    turn.reads.push(shown);
                }
            }
            Kind::Text => {
                let remaining = MAX_CONTEXT_BYTES.saturating_sub(blocks.len());
                let (block, note) = file_block(&path, &shown, remaining)?;
                blocks.push_str(&block);
                turn.notes.push(note);
                turn.reads.push(shown);
            }
            Kind::Image | Kind::Document => {
                check_upload_size(&path)?;
                turn.notes.push(format!("Attached {shown}"));
                turn.uploads.push(path);
            }
            Kind::Unsupported => bail!(
                "@{mention} is a binary file the chat can't read (it takes text, pdf, xlsx, xls, and png/jpg/webp/gif)"
            ),
        }
    }
    for (index, image) in images.iter().enumerate() {
        let chip = image_chip(index + 1);
        if !text.contains(&chip) {
            continue;
        }
        check_upload_size(image)?;
        let name = image
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "image".to_string());
        turn.notes.push(format!("Attached {chip} ({name})"));
        turn.uploads.push(image.clone());
    }
    if !blocks.is_empty() {
        turn.prompt = format!("{text}\n\n{}", blocks.trim_end());
    }
    Ok(turn)
}

fn check_upload_size(path: &Path) -> Result<()> {
    let size = std::fs::metadata(path)
        .with_context(|| format!("cannot read {}", path.display()))?
        .len();
    if size > MAX_UPLOAD_BYTES {
        bail!(
            "{} is {} MB; the chat accepts files up to {} MB",
            path.display(),
            size / (1024 * 1024),
            MAX_UPLOAD_BYTES / (1024 * 1024)
        );
    }
    Ok(())
}

/// One file as `<file path="…">…</file>`, capped by lines and by the bytes
/// left in this turn's budget; the note says what was kept.
fn file_block(path: &Path, shown: &str, budget: usize) -> Result<(String, String)> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    let total = text.lines().count();
    let mut kept = String::new();
    let mut lines = 0;
    for line in text.lines().take(MAX_FILE_LINES) {
        if kept.len() + line.len() + 1 > budget.saturating_sub(64) {
            break;
        }
        kept.push_str(line);
        kept.push('\n');
        lines += 1;
    }
    let note = if lines == total {
        format!("Read {shown} ({total} lines)")
    } else {
        format!("Read {shown} (first {lines} of {total} lines)")
    };
    let truncated = if lines < total {
        format!("[… {} more lines not included]\n", total - lines)
    } else {
        String::new()
    };
    Ok((
        format!("<file path=\"{shown}\">\n{kept}{truncated}</file>\n"),
        note,
    ))
}

/// A folder's direct entries (gitignore-aware, secrets hidden).
fn listing(sandbox: &Sandbox, dir: &Path) -> Result<(String, usize)> {
    let mut entries: Vec<String> = WalkBuilder::new(dir)
        .hidden(false)
        .require_git(false)
        .max_depth(Some(1))
        .filter_entry(|e| e.file_name() != ".git")
        .build()
        .filter_map(|e| e.ok())
        .filter(|e| e.depth() == 1 && !sandbox.is_secret(e.path()))
        .map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            if e.file_type().is_some_and(|t| t.is_dir()) {
                format!("{name}/")
            } else {
                name
            }
        })
        .collect();
    entries.sort();
    let count = entries.len();
    entries.truncate(MAX_LISTING);
    let shown = sandbox.display(dir);
    let more = count.saturating_sub(MAX_LISTING);
    let tail = if more > 0 {
        format!("[… {more} more]\n")
    } else {
        String::new()
    };
    Ok((
        format!(
            "<directory path=\"{shown}/\">\n{}\n{tail}</directory>\n",
            entries.join("\n")
        ),
        count,
    ))
}

/// Uploads one file through chat's document flow (presigned S3 PUT with the
/// content type the URL was signed for, then registration) and returns the
/// registration reply, which carries `documentId`.
pub async fn upload(http: &ApiClient, path: &Path) -> Result<Value> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .context("the upload needs a file name")?;
    let bytes = std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    let query = [("filename".to_string(), name)];
    let ticket = http
        .request_json(
            CHAT,
            Method::POST,
            "/chat/upload/presigned-url",
            &query,
            None,
        )
        .await?;
    let upload_url = ticket["uploadUrl"]
        .as_str()
        .context("the server did not return an upload URL")?;
    let content_type = ticket["contentType"]
        .as_str()
        .context("the server did not say which content type it signed")?;
    http.put_bytes(upload_url, content_type, bytes).await?;
    let done = json!({"s3Key": ticket["s3Key"], "bucket": ticket["bucket"]});
    http.request_json(CHAT, Method::POST, "/chat/upload", &[], Some(done))
        .await
}

/// The document id from an upload reply.
pub fn document_id(reply: &Value) -> Result<String> {
    reply["documentId"]
        .as_str()
        .map(str::to_string)
        .with_context(|| {
            let message = reply["message"]
                .as_str()
                .unwrap_or("no document id returned");
            format!("the upload wasn't accepted: {message}")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(name: &str) -> (PathBuf, Sandbox) {
        let dir = std::env::temp_dir().join(format!(
            "gz-attach-{name}-{}-{}",
            std::process::id(),
            crate::cmd::new_uuid()
        ));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/math.rs"), "pub fn add() {}\n// two\n").unwrap();
        std::fs::write(dir.join("notes with space.md"), "hello\n").unwrap();
        std::fs::write(dir.join(".env"), "SECRET=1\n").unwrap();
        std::fs::write(dir.join("chart.png"), [0x89, b'P', b'N', b'G', 0]).unwrap();
        std::fs::write(dir.join("blob.bin"), [0u8, 1, 2, 3]).unwrap();
        let sandbox = Sandbox::new(&dir).unwrap();
        (dir, sandbox)
    }

    #[test]
    fn parses_mentions_quotes_and_skips_emails() {
        let text =
            "look at @src/math.rs, and @\"notes with space.md\" (mail me@x.com) @src/math.rs";
        assert_eq!(
            parse_mentions(text),
            vec!["src/math.rs", "notes with space.md"]
        );
        assert!(parse_mentions("no mentions @ here").is_empty());
    }

    #[test]
    fn inlines_text_and_lists_folders() {
        let (_dir, sandbox) = workspace("inline");
        let turn = prepare(&sandbox, "explain @src/math.rs and @src", &[]).unwrap();
        assert!(turn.prompt.starts_with("explain @src/math.rs and @src\n\n"));
        assert!(turn
            .prompt
            .contains("<file path=\"src/math.rs\">\npub fn add() {}\n// two\n</file>"));
        assert!(turn
            .prompt
            .contains("<directory path=\"src/\">\nmath.rs\n</directory>"));
        assert_eq!(
            turn.notes,
            vec!["Read src/math.rs (2 lines)", "Listed src/ (1 entries)"]
        );
        assert_eq!(turn.display, "explain @src/math.rs and @src");
        assert!(turn.uploads.is_empty());
    }

    #[test]
    fn caps_long_files_and_notes_it() {
        let (dir, sandbox) = workspace("caps");
        let long: String = (0..2500).map(|i| format!("line {i}\n")).collect();
        std::fs::write(dir.join("big.log"), long).unwrap();
        let turn = prepare(&sandbox, "@big.log", &[]).unwrap();
        assert_eq!(turn.notes, vec!["Read big.log (first 2000 of 2500 lines)"]);
        assert!(turn.prompt.contains("[… 500 more lines not included]"));
    }

    #[test]
    fn refuses_secrets_missing_paths_and_unknown_binaries() {
        let (_dir, sandbox) = workspace("refuse");
        let secret = prepare(&sandbox, "@.env", &[]).unwrap_err();
        assert!(format!("{secret:#}").contains("blocked"), "{secret:#}");
        let missing = prepare(&sandbox, "@nope.rs", &[]).unwrap_err();
        assert!(missing.to_string().contains("doesn't exist"));
        let binary = prepare(&sandbox, "@blob.bin", &[]).unwrap_err();
        assert!(binary.to_string().contains("binary"));
        let outside = prepare(&sandbox, "@../../etc/hosts", &[]).unwrap_err();
        assert!(format!("{outside:#}").contains("outside"), "{outside:#}");
    }

    #[test]
    fn images_go_to_uploads_by_mention_or_chip() {
        let (dir, sandbox) = workspace("images");
        let pasted = dir.join("chart.png");
        let turn = prepare(
            &sandbox,
            "what is in @chart.png and [Image #1]",
            &[pasted.clone()],
        )
        .unwrap();
        assert_eq!(turn.uploads.len(), 2);
        assert_eq!(turn.notes[0], "Attached chart.png");
        assert_eq!(turn.notes[1], "Attached [Image #1] (chart.png)");
        assert_eq!(turn.prompt, turn.display, "images are not inlined");
        let unused = prepare(&sandbox, "no chip here", &[pasted]).unwrap();
        assert!(unused.uploads.is_empty(), "a removed chip isn't uploaded");
    }

    #[test]
    fn classifies_by_extension_then_content() {
        let (dir, _sandbox) = workspace("classify");
        assert_eq!(classify(&dir.join("src")).unwrap(), Kind::Directory);
        assert_eq!(classify(&dir.join("chart.png")).unwrap(), Kind::Image);
        assert_eq!(classify(&dir.join("src/math.rs")).unwrap(), Kind::Text);
        assert_eq!(classify(&dir.join("blob.bin")).unwrap(), Kind::Unsupported);
        std::fs::write(dir.join("r.pdf"), b"%PDF").unwrap();
        assert_eq!(classify(&dir.join("r.pdf")).unwrap(), Kind::Document);
    }

    #[test]
    fn reads_document_ids_from_upload_replies() {
        assert_eq!(
            document_id(&json!({"documentId": "d1", "status": "success"})).unwrap(),
            "d1"
        );
        let err = document_id(&json!({"error": "x", "message": "too big"})).unwrap_err();
        assert!(err.to_string().contains("too big"));
    }
}
