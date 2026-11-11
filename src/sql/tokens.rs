//! A SQL tokenizer for reading a query's shape without a full parser: the
//! words, quoted names and punctuation it is made of, with strings,
//! numbers and comments reduced to what cannot be a name.
//!
//! Queries come from the lakehouse (Spark SQL) and from Designer datasets
//! (Spark, or DuckDB through `readGzLakehouse`). `'…'` is a string;
//! `"…"` and `` `…` `` are quoted names, since a dataset quotes its names
//! with either and a string can never stand where a name goes. A backslash
//! escapes the next character in `'…'` and `"…"`, as in Spark, so an
//! escaped quote does not end them. `{{…}}` is a Designer placeholder.

use std::ops::Range;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token {
    /// A bare word: keyword or unquoted name.
    Word(String),
    /// A `"quoted"` or `` `quoted` `` name, unquoted.
    Quoted(String),
    /// A string or number; its text does not matter here.
    Literal,
    /// A `{{...}}` Designer placeholder, as written.
    Placeholder(String),
    /// Any other single character: `.`, `,`, `(`, `)`, `;`, operators.
    Punct(char),
}

impl Token {
    /// The word in upper case, when this is a bare word.
    pub fn keyword(&self) -> Option<String> {
        match self {
            Token::Word(word) => Some(word.to_ascii_uppercase()),
            _ => None,
        }
    }

    pub fn is_keyword(&self, wanted: &str) -> bool {
        self.keyword().as_deref() == Some(wanted)
    }
}

/// A token and the char range it covers in the text.
pub type Spanned = (Token, Range<usize>);

pub fn tokenize(sql: &str) -> Vec<Token> {
    spanned_tokens(sql)
        .into_iter()
        .map(|(token, _)| token)
        .collect()
}

/// The tokens of `sql`, each with its char range.
pub fn spanned_tokens(sql: &str) -> Vec<Spanned> {
    let chars: Vec<char> = sql.chars().collect();
    let mut tokens = Vec::new();
    let mut at = 0;
    while let Some(&ch) = chars.get(at) {
        let next = chars.get(at + 1).copied();
        let (token, end) = match ch {
            _ if ch.is_whitespace() => (None, at + 1),
            '-' if next == Some('-') => (None, line_end(&chars, at)),
            '/' if next == Some('*') => (None, past(&chars, at + 2, &['*', '/'])),
            '{' if next == Some('{') => {
                let end = past(&chars, at + 2, &['}', '}']);
                let text = chars[at..end].iter().collect();
                (Some(Token::Placeholder(text)), end)
            }
            '\'' => (Some(Token::Literal), past_quote(&chars, at).0),
            '"' | '`' => {
                let (end, text) = past_quote(&chars, at);
                (Some(Token::Quoted(text)), end)
            }
            // A number such as 1.5e3 is one token, so its dot is no name's.
            _ if ch.is_ascii_digit() => (Some(Token::Literal), number_end(&chars, at)),
            _ if ch.is_alphabetic() || ch == '_' || ch == '$' => {
                let end = word_end(&chars, at);
                let text = chars[at..end].iter().collect();
                (Some(Token::Word(text)), end)
            }
            _ => (Some(Token::Punct(ch)), at + 1),
        };
        if let Some(token) = token {
            tokens.push((token, at..end));
        }
        at = end;
    }
    tokens
}

fn word_end(chars: &[char], start: usize) -> usize {
    end_of_run(chars, start, |c| {
        c.is_alphanumeric() || c == '_' || c == '$'
    })
}

fn number_end(chars: &[char], start: usize) -> usize {
    end_of_run(chars, start, |c| {
        c.is_alphanumeric() || c == '.' || c == '_'
    })
}

fn end_of_run(chars: &[char], start: usize, belongs: impl Fn(char) -> bool) -> usize {
    (start..chars.len())
        .find(|&at| !belongs(chars[at]))
        .unwrap_or(chars.len())
}

/// The position of the newline ending the line comment at `start`.
fn line_end(chars: &[char], start: usize) -> usize {
    (start..chars.len())
        .find(|&at| chars[at] == '\n')
        .unwrap_or(chars.len())
}

/// The position after `marker`, searching from `from`; the end of the text
/// when it never comes.
fn past(chars: &[char], from: usize, marker: &[char]) -> usize {
    (from..chars.len())
        .find(|&at| chars[at..].starts_with(marker))
        .map_or(chars.len(), |at| at + marker.len())
}

/// The position after the quoted text opening at `start`, and its text. A
/// doubled quote stays inside; outside backticks a backslash escapes the
/// next character.
fn past_quote(chars: &[char], start: usize) -> (usize, String) {
    let quote = chars[start];
    let mut text = String::new();
    let mut at = start + 1;
    while let Some(&ch) = chars.get(at) {
        if ch == '\\' && quote != '`' {
            if let Some(&escaped) = chars.get(at + 1) {
                text.push(escaped);
            }
            at += 2;
            continue;
        }
        if ch == quote {
            if chars.get(at + 1) == Some(&quote) {
                text.push(quote);
                at += 2;
                continue;
            }
            return (at + 1, text);
        }
        text.push(ch);
        at += 1;
    }
    (chars.len(), text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_comments_and_numbers_hide_their_text() {
        let tokens = tokenize("SELECT 'a FROM b', 1.5e3 -- FROM c\n/* FROM d */ x");
        assert_eq!(
            tokens,
            [
                Token::Word("SELECT".into()),
                Token::Literal,
                Token::Punct(','),
                Token::Literal,
                Token::Word("x".into()),
            ]
        );
    }

    #[test]
    fn a_backslash_escapes_a_quote_in_strings_and_double_quotes_but_not_backticks() {
        let tokens = tokenize(r#"'it\'s' "say \"hi\"" `a\` x"#);
        assert_eq!(
            tokens,
            [
                Token::Literal,
                Token::Quoted("say \"hi\"".into()),
                Token::Quoted("a\\".into()),
                Token::Word("x".into()),
            ]
        );
    }

    #[test]
    fn placeholders_keep_their_text_and_spans_cover_each_token() {
        let sql = "IN {{Region.value}}";
        let spanned = spanned_tokens(sql);
        assert_eq!(spanned[1].0, Token::Placeholder("{{Region.value}}".into()));
        assert_eq!(spanned[1].1, 3..sql.len());
    }
}
