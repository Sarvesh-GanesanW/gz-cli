//! Syntax colours for the command or script shown in an approval prompt.
//! The prompt wraps long lines onto extra rows; the code is highlighted as
//! a whole and each row takes the colours of the text it shows.

use ratatui::style::Style;
use syntect::parsing::SyntaxReference;

use crate::assistant::tools::Access;
use crate::tui::highlight::{self, Surface};

/// The grammar for a prompt's preview: shell for commands, the script's
/// language (`run_script python (11 lines)`) for scripts.
pub fn syntax(access: Access, summary: &str) -> Option<&'static SyntaxReference> {
    match access {
        Access::Command => highlight::syntax_for_tag("sh"),
        Access::Script => {
            let language = summary.split_whitespace().nth(1)?;
            highlight::syntax_for_tag(language)
        }
        _ => None,
    }
}

/// Each wrapped row of `preview` as styled runs on `base`. `rows` are the
/// row texts in order, each a piece of the preview (tabs shown as four
/// spaces); a row that can't be placed in the text stays in `base`.
pub fn styled_rows(
    preview: &str,
    syntax: Option<&'static SyntaxReference>,
    rows: &[&str],
    base: Style,
) -> Vec<Vec<(String, Style)>> {
    let plain = |row: &str| vec![(row.to_string(), base)];
    let Some(syntax) = syntax else {
        return rows.iter().map(|row| plain(row)).collect();
    };
    let text = preview.replace('\t', "    ");
    let lines = highlight::highlight(&text, syntax);
    // Every character of the text with its line's tokens laid out flat, so
    // a row found at a byte offset can take the styles found there.
    let mut kinds = Vec::with_capacity(text.len());
    for tokens in &lines {
        for (piece, kind) in tokens {
            kinds.extend(std::iter::repeat_n(*kind, piece.len()));
        }
        kinds.push(highlight::Kind::Plain);
    }
    let mut cursor = 0;
    rows.iter()
        .map(|row| {
            let body = row.trim_start();
            let indent = &row[..row.len() - body.len()];
            let Some(found) = text.get(cursor..).and_then(|rest| rest.find(body)) else {
                return plain(row);
            };
            let start = cursor + found;
            cursor = start + body.len();
            let mut runs = vec![(indent.to_string(), base)];
            for (offset, ch) in body.char_indices() {
                let kind = kinds
                    .get(start + offset)
                    .copied()
                    .unwrap_or(highlight::Kind::Plain);
                let style = highlight::style(kind, Surface::Plain, base);
                match runs.last_mut() {
                    Some((last, last_style)) if *last_style == style => last.push(ch),
                    _ => runs.push((ch.to_string(), style)),
                }
            }
            runs.retain(|(t, _)| !t.is_empty());
            runs
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::theme;

    #[test]
    fn picks_shell_for_commands_and_the_scripts_language() {
        assert_eq!(
            syntax(Access::Command, "run_command ls").unwrap().name,
            "Bourne Again Shell (bash)"
        );
        assert_eq!(
            syntax(Access::Script, "run_script python (3 lines)")
                .unwrap()
                .name,
            "Python"
        );
        assert_eq!(
            syntax(Access::Script, "run_script node (3 lines)")
                .unwrap()
                .name,
            "JavaScript"
        );
        assert!(syntax(Access::Read, "read_file a").is_none());
    }

    #[test]
    fn wrapped_rows_keep_their_text_and_take_the_codes_colours() {
        let preview = "x = \"a long string\"\n\tprint(x)";
        let rows = ["x = \"a long", "string\"", "    print(x)"];
        let styled = styled_rows(
            preview,
            syntax(Access::Script, "run_script python"),
            &rows,
            Style::default(),
        );
        for (row, runs) in rows.iter().zip(&styled) {
            let text: String = runs.iter().map(|(t, _)| t.as_str()).collect();
            assert_eq!(&text, row);
        }
        let style_of = |row: usize, needle: &str| {
            styled[row]
                .iter()
                .find(|(t, _)| t.contains(needle))
                .unwrap()
                .1
        };
        assert_eq!(
            style_of(1, "string").fg,
            Some(theme::OK),
            "a wrapped string stays a string"
        );
        assert_eq!(style_of(2, "print").fg, Some(theme::ACCENT));
    }

    #[test]
    fn unknown_languages_and_unplaced_rows_stay_in_the_base_style() {
        let bold = Style::default().add_modifier(ratatui::style::Modifier::BOLD);
        assert_eq!(
            styled_rows("ls", None, &["ls"], bold),
            vec![vec![("ls".to_string(), bold)]]
        );
        let styled = styled_rows(
            "ls -la",
            syntax(Access::Command, ""),
            &["nothing like it"],
            bold,
        );
        assert_eq!(styled, vec![vec![("nothing like it".to_string(), bold)]]);
    }
}
