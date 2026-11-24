//! Text changes that keep a file's encoding details: a UTF-8 byte-order
//! mark, Windows (CRLF) line endings, and bytes that are not UTF-8 at all.
//! `read_file` shows text with the mark invisible and undecodable bytes as
//! U+FFFD, so changes worked out from what it showed must never write
//! those stand-ins back over the real bytes.

use anyhow::{bail, Result};

const BOM: &str = "\u{FEFF}";
const REPLACEMENT: char = '\u{FFFD}';

/// What `read_file` should say about a file's bytes before its lines, if
/// anything.
pub fn read_note(bytes: &[u8]) -> Option<&'static str> {
    std::str::from_utf8(bytes).is_err().then_some(
        "(this file is not valid UTF-8: bytes that are not UTF-8 show as U+FFFD below; \
         edit_file can't change it, so change it with run_script working on the bytes)",
    )
}

/// The file's text for `tool` (`edit_file`, `notebook_edit`) to change,
/// refused when it isn't UTF-8: writing it back would turn every
/// undecodable byte into U+FFFD.
pub fn editable(bytes: Vec<u8>, shown: &str, tool: &str) -> Result<String> {
    match String::from_utf8(bytes) {
        Ok(text) => Ok(text),
        Err(err) => bail!(
            "{shown} is not valid UTF-8 (byte {} is not), so {tool} would corrupt it; \
             change it with run_script, reading and writing bytes (for example Python's \
             open(path, 'rb'))",
            err.utf8_error().valid_up_to()
        ),
    }
}

/// Exact-string replacement in `old`, keeping its byte-order mark and its
/// CRLF line endings: `find` and `replace` written with `\n` match and
/// write `\r\n` in a CRLF file. In a file that mixes CRLF and LF, `find`
/// matches whatever endings the file has there, since `read_file` never
/// shows them (see [`replace_ignoring_endings`]). Ambiguity is an error so
/// the model must be specific (or ask for `replace_all`).
pub fn apply_edit(
    old: &str,
    find: &str,
    replace: &str,
    replace_all: bool,
) -> Result<(String, usize)> {
    let (bom, body) = split_bom(old);
    let find = find.strip_prefix(BOM).unwrap_or(find);
    if find.is_empty() {
        bail!("old_text is empty");
    }
    let (find, replace) = match is_crlf(body) {
        true => (to_crlf(find), to_crlf(replace)),
        false => (find.to_string(), replace.to_string()),
    };
    let (content, count) = match body.matches(find.as_str()).count() {
        0 if body.contains("\r\n") && find.contains('\n') => {
            replace_ignoring_endings(body, &find, &replace, replace_all)?
        }
        0 => bail!("old_text was not found in the file"),
        1 => (body.replacen(find.as_str(), &replace, 1), 1),
        n if replace_all => (body.replace(find.as_str(), &replace), n),
        n => bail!("old_text appears {n} times; include more context or set replace_all"),
    };
    Ok((format!("{bom}{content}"), count))
}

/// Where in a mixed-endings file `find` matched once its endings are set
/// aside, as byte ranges of the original text.
fn spans_ignoring_endings(body: &str, find: &str) -> Vec<(usize, usize)> {
    // `plain` is `body` without the `\r` of each CRLF; `origin[i]` is
    // where `plain`'s byte `i` sits in `body`.
    let mut plain = String::with_capacity(body.len());
    let mut origin = Vec::with_capacity(body.len());
    for (index, c) in body.char_indices() {
        if c == '\r' && body[index + 1..].starts_with('\n') {
            continue;
        }
        plain.push(c);
        origin.extend(index..index + c.len_utf8());
    }
    let find = find.replace("\r\n", "\n");
    plain
        .match_indices(find.as_str())
        .map(|(start, found)| {
            let mut from = origin[start];
            // A match opening on a line break takes that break's `\r` too.
            if found.starts_with('\n') && body[..from].ends_with('\r') {
                from -= 1;
            }
            (from, origin[start + found.len() - 1] + 1)
        })
        .collect()
}

/// [`apply_edit`]'s fallback for a file mixing CRLF and LF: matches `find`
/// whatever the endings at each match, and writes `replace` with CRLF
/// where every line break it replaces was CRLF, as given otherwise.
fn replace_ignoring_endings(
    body: &str,
    find: &str,
    replace: &str,
    replace_all: bool,
) -> Result<(String, usize)> {
    let spans = spans_ignoring_endings(body, find);
    match spans.len() {
        0 => bail!(
            "old_text was not found in the file. This file mixes CRLF and LF line endings, \
             which read_file doesn't show; gz matched old_text with either ending, so the \
             text itself differs: read the lines again and copy them exactly"
        ),
        1 => {}
        _ if replace_all => {}
        n => bail!("old_text appears {n} times; include more context or set replace_all"),
    }
    let mut content = String::with_capacity(body.len());
    let mut done = 0;
    for &(from, to) in &spans {
        content.push_str(&body[done..from]);
        let replaced = &body[from..to];
        match is_crlf(replaced) {
            true => content.push_str(&to_crlf(replace)),
            false => content.push_str(replace),
        }
        done = to;
    }
    content.push_str(&body[done..]);
    Ok((content, spans.len()))
}

/// What `write_file` writes: the content, and a note for the model when
/// gz kept the old file's line endings or byte-order mark in it.
#[derive(Debug, PartialEq, Eq)]
pub struct Replacement {
    pub content: String,
    pub kept: Option<String>,
}

/// The content `write_file` puts over a file that held `old` (`None`: a
/// new file). The new content keeps the old file's byte-order mark and
/// CRLF line endings, and `kept` says so when that changed what the model
/// sent. Over a file that is not UTF-8 the content is a deliberate
/// replacement only when it holds no U+FFFD: with one, it was copied from
/// what `read_file` showed, and writing it would destroy the bytes U+FFFD
/// stands for.
pub fn replacement(old: Option<&[u8]>, content: &str, shown: &str) -> Result<Replacement> {
    let as_sent = |content: &str| Replacement {
        content: content.to_string(),
        kept: None,
    };
    let Some(old) = old else {
        return Ok(as_sent(content));
    };
    let Ok(old) = std::str::from_utf8(old) else {
        if content.contains(REPLACEMENT) {
            bail!(
                "{shown} is not valid UTF-8, and the new content holds U+FFFD, which is what \
                 read_file shows in place of bytes it could not decode: writing it would \
                 destroy those bytes. Change the file with run_script working on the bytes, \
                 or write content without U+FFFD if you mean to replace all of it"
            );
        }
        return Ok(as_sent(content));
    };
    let (bom, body) = split_bom(old);
    let sent_bom = content.starts_with(BOM);
    let content = content.strip_prefix(BOM).unwrap_or(content);
    let to_crlf_endings = is_crlf(body) && !content.contains("\r\n") && content.contains('\n');
    let mut kept = Vec::new();
    if to_crlf_endings {
        kept.push("CRLF line endings");
    }
    if !bom.is_empty() && !sent_bom {
        kept.push("UTF-8 byte-order mark");
    }
    let content = match to_crlf_endings {
        true => to_crlf(content),
        false => content.to_string(),
    };
    let kept = (!kept.is_empty()).then(|| {
        format!(
            "kept the file's {}; to change that, rewrite it with run_script",
            kept.join(" and ")
        )
    });
    Ok(Replacement {
        content: format!("{bom}{content}"),
        kept,
    })
}

fn split_bom(text: &str) -> (&str, &str) {
    match text.strip_prefix(BOM) {
        Some(rest) => (BOM, rest),
        None => ("", text),
    }
}

/// Whether every line break in `text` is CRLF (and there is at least one).
fn is_crlf(text: &str) -> bool {
    let breaks = text.matches('\n').count();
    breaks > 0 && text.matches("\r\n").count() == breaks
}

fn to_crlf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\n', "\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_requires_a_unique_match() {
        assert!(apply_edit("a a", "a", "b", false).is_err());
        assert_eq!(
            apply_edit("a a", "a", "b", true).unwrap(),
            ("b b".to_string(), 2)
        );
        assert!(apply_edit("abc", "zzz", "b", false).is_err());
        assert!(apply_edit("abc", "", "b", false).is_err());
    }

    #[test]
    fn crlf_files_match_lf_old_text_and_stay_crlf() {
        let old = "fn a() {\r\n    1\r\n}\r\n";
        let (new, n) = apply_edit(old, "{\n    1\n}", "{\n    2\n    3\n}", false).unwrap();
        assert_eq!(n, 1);
        assert_eq!(new, "fn a() {\r\n    2\r\n    3\r\n}\r\n");
        // Text written with CRLF already works as is.
        let (new, _) = apply_edit(old, "1\r\n", "9\r\n", false).unwrap();
        assert_eq!(new, "fn a() {\r\n    9\r\n}\r\n");
    }

    #[test]
    fn lf_and_mixed_files_are_matched_exactly() {
        let (new, _) = apply_edit("a\nb\n", "a\nb", "x\ny", false).unwrap();
        assert_eq!(new, "x\ny\n");
        let mixed = "a\r\nb\nc\r\n";
        let (new, _) = apply_edit(mixed, "b\nc", "q\nr", false).unwrap();
        assert_eq!(new, "a\r\nq\nr\r\n");
    }

    #[test]
    fn mixed_files_match_across_endings_read_file_hides() {
        let mixed = "a\r\nb\nc\r\n";
        let (new, n) = apply_edit(mixed, "a\nb", "z", false).unwrap();
        assert_eq!((new.as_str(), n), ("z\nc\r\n", 1));
        // Every break replaced was CRLF, so the new text's are too.
        let (new, _) = apply_edit("a\r\nb\r\nc\n", "a\nb\n", "x\ny\n", false).unwrap();
        assert_eq!(new, "x\r\ny\r\nc\n");
        // A match opening on a line break takes the whole CRLF.
        let (new, _) = apply_edit("a\r\nb\r\nc\nd", "\nb\nc", "\nB\nC", false).unwrap();
        assert_eq!(new, "a\r\nB\r\nC\nd");
        let (new, n) = apply_edit("é\r\nx\né\r\nx\n", "é\nx", "y", true).unwrap();
        assert_eq!((new.as_str(), n), ("y\ny\n", 2));
        let twice = apply_edit("é\r\nx\né\r\nx\n", "é\nx", "y", false).unwrap_err();
        assert!(twice.to_string().contains("appears 2 times"), "{twice}");
        let missing = apply_edit(mixed, "a\nq", "z", false).unwrap_err();
        assert!(
            missing.to_string().contains("mixes CRLF and LF"),
            "{missing}"
        );
    }

    #[test]
    fn the_byte_order_mark_and_final_newline_stay() {
        let old = "\u{FEFF}first\nsecond";
        let (new, _) = apply_edit(old, "first", "1st", false).unwrap();
        assert_eq!(new, "\u{FEFF}1st\nsecond");
        // old_text copied with the invisible mark still matches.
        let (new, _) = apply_edit(old, "\u{FEFF}first", "1st", false).unwrap();
        assert_eq!(new, "\u{FEFF}1st\nsecond");
        let (new, _) = apply_edit("a\n", "a", "b", false).unwrap();
        assert_eq!(new, "b\n");
    }

    #[test]
    fn files_that_are_not_utf8_are_never_edited() {
        let err = editable(b"caf\xe9\n".to_vec(), "menu.txt", "edit_file").unwrap_err();
        let text = err.to_string();
        assert!(text.contains("not valid UTF-8 (byte 3"), "{text}");
        assert!(text.contains("so edit_file would"), "{text}");
        assert!(text.contains("run_script"), "{text}");
        let err = editable(b"\xff".to_vec(), "a.ipynb", "notebook_edit").unwrap_err();
        assert!(err.to_string().contains("so notebook_edit would"), "{err}");
        assert_eq!(editable(b"ok".to_vec(), "a", "edit_file").unwrap(), "ok");
        assert!(read_note(b"caf\xe9").is_some());
        assert!(read_note("café".as_bytes()).is_none());
    }

    #[test]
    fn overwrites_say_what_they_kept() {
        let crlf = replacement(Some(b"x\r\ny\r\n"), "a\nb\n", "f").unwrap();
        let note = crlf.kept.unwrap();
        assert!(note.contains("CRLF line endings"), "{note}");
        assert!(note.contains("run_script"), "{note}");
        let marked = replacement(Some("\u{FEFF}x\r\n".as_bytes()), "a\n", "f").unwrap();
        assert!(marked
            .kept
            .unwrap()
            .contains("CRLF line endings and UTF-8 byte-order mark"));
        // Nothing changed from what was sent: nothing to say.
        for (old, sent) in [
            (&b"x\r\n"[..], "a\r\n"),
            (&b"x\n"[..], "a\n"),
            ("\u{FEFF}x".as_bytes(), "\u{FEFF}a"),
            (&b"x\r\n"[..], "one line"),
        ] {
            assert_eq!(
                replacement(Some(old), sent, "f").unwrap().kept,
                None,
                "{sent:?}"
            );
        }
        assert_eq!(replacement(None, "a\n", "f").unwrap().kept, None);
    }

    #[test]
    fn overwrites_keep_endings_and_mark_but_never_write_stand_ins() {
        assert_eq!(replacement(None, "a\n", "f").unwrap().content, "a\n");
        assert_eq!(
            replacement(Some(b"x\r\ny\r\n"), "a\nb\n", "f")
                .unwrap()
                .content,
            "a\r\nb\r\n"
        );
        assert_eq!(
            replacement(Some(b"x\ny\n"), "a\nb\n", "f").unwrap().content,
            "a\nb\n"
        );
        assert_eq!(
            replacement(Some("\u{FEFF}x\n".as_bytes()), "a\n", "f")
                .unwrap()
                .content,
            "\u{FEFF}a\n"
        );
        assert_eq!(
            replacement(Some("\u{FEFF}x\n".as_bytes()), "\u{FEFF}a\n", "f")
                .unwrap()
                .content,
            "\u{FEFF}a\n"
        );

        let latin1 = b"caf\xe9\n";
        let err = replacement(Some(latin1), "caf\u{FFFD}!\n", "menu.txt").unwrap_err();
        assert!(
            err.to_string().contains("would destroy those bytes"),
            "{err}"
        );
        assert_eq!(
            replacement(Some(latin1), "café!\n", "menu.txt")
                .unwrap()
                .content,
            "café!\n",
            "a whole new UTF-8 content is a deliberate replacement"
        );
    }
}
