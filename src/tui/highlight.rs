//! Syntax highlighting for code shown in the terminal: fenced blocks in
//! answers, diffs, and file previews.
//!
//! Grammars come from two-face's set (bat's, a superset of syntect's);
//! no colour theme is used.
//! Each token is sorted into a [`Kind`] (keyword, string, comment, …) and the
//! kind picks one of the palette's ANSI colours, so highlighted code reads the
//! same in Terminal.app and on light or dark backgrounds. Work is bounded:
//! only the first [`MAX_HIGHLIGHT_BYTES`] of a block are highlighted (the rest
//! stays plain), finished blocks are cached because the transcript view
//! redraws them every frame, and a block that grows while an answer streams
//! is highlighted only where it grew.

use std::cell::RefCell;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::OnceLock;

use ratatui::style::{Color, Modifier, Style};
use syntect::highlighting::{
    Color as ThemeColor, HighlightIterator, HighlightState, Highlighter, ScopeSelectors,
    StyleModifier, Theme, ThemeItem, ThemeSettings,
};
use syntect::parsing::{ParseState, ScopeStack, SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;

use crate::tui::theme;

/// Bytes of one block that get highlighted; the rest is shown plain so a
/// huge file or answer never stalls a redraw.
const MAX_HIGHLIGHT_BYTES: usize = 32 * 1024;
/// A line longer than this (minified code, data) ends highlighting for the
/// block: the regex grammars slow down sharply on such lines.
const MAX_HIGHLIGHT_LINE: usize = 2_000;
/// Memory the highlight cache may hold (code plus tokens, estimated): the
/// transcript view draws every block of the session on each frame, so
/// finished blocks are remembered, least recently drawn dropped first.
const CACHE_BUDGET_BYTES: usize = 8 * 1024 * 1024;
/// Bigger blocks (a whole file in the transcript) aren't cached: they are
/// drawn rarely and would push many small blocks out.
const MAX_CACHED_BYTES: usize = 4 * MAX_HIGHLIGHT_BYTES;

/// What a token is, as far as colouring goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Plain,
    Comment,
    Keyword,
    String,
    Constant,
    Function,
    Type,
    Heading,
    Inserted,
    Deleted,
    Invalid,
}

/// Where the code is drawn: on the terminal's own background, or on a
/// coloured diff bar that needs brighter text to stay readable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    Plain,
    Bar,
}

/// One coloured piece of a line.
pub type Token = (String, Kind);

/// Scope selectors for each kind, most specific first: syntect picks the
/// best-matching selector, and the kind is carried in the colour's red byte.
const SELECTORS: &[(&str, Kind)] = &[
    ("comment, punctuation.definition.comment", Kind::Comment),
    ("string, punctuation.definition.string", Kind::String),
    (
        "constant.numeric, constant.language, constant.character, constant.other, support.constant",
        Kind::Constant,
    ),
    (
        "keyword, storage, keyword.operator.word, variable.language",
        Kind::Keyword,
    ),
    ("keyword.operator, punctuation", Kind::Plain),
    (
        "entity.name.function, support.function, meta.function-call variable.function, variable.function",
        Kind::Function,
    ),
    (
        "entity.name.type, entity.name.class, entity.name.struct, entity.name.enum, \
         entity.name.trait, entity.other.inherited-class, support.type, support.class",
        Kind::Type,
    ),
    ("entity.name.tag", Kind::Keyword),
    ("entity.other.attribute-name", Kind::Function),
    ("markup.heading, entity.name.section", Kind::Heading),
    ("markup.inserted", Kind::Inserted),
    ("markup.deleted", Kind::Deleted),
    ("invalid", Kind::Invalid),
];

const KINDS: [Kind; 11] = [
    Kind::Plain,
    Kind::Comment,
    Kind::Keyword,
    Kind::String,
    Kind::Constant,
    Kind::Function,
    Kind::Type,
    Kind::Heading,
    Kind::Inserted,
    Kind::Deleted,
    Kind::Invalid,
];

/// Fence tags and file extensions the grammars don't know by name, mapped
/// to the closest one they have.
const ALIASES: &[(&str, &str)] = &[
    ("jsx", "js"),
    ("mjs", "js"),
    ("cjs", "js"),
    ("javascriptreact", "js"),
    ("node", "js"),
    ("jsonc", "json"),
    ("json5", "json"),
    ("shell", "sh"),
    ("console", "sh"),
    ("shellscript", "sh"),
    ("ksh", "sh"),
    ("plist", "xml"),
    ("hql", "sql"),
    ("psql", "sql"),
    ("pgsql", "sql"),
    ("sparksql", "sql"),
    ("mysql", "sql"),
    ("python3", "py"),
];

fn syntaxes() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(two_face::syntax::extra_newlines)
}

fn palette_theme() -> &'static Theme {
    static THEME: OnceLock<Theme> = OnceLock::new();
    THEME.get_or_init(|| Theme {
        settings: ThemeSettings {
            foreground: Some(kind_colour(Kind::Plain)),
            ..ThemeSettings::default()
        },
        scopes: SELECTORS
            .iter()
            .filter_map(|(selector, kind)| {
                Some(ThemeItem {
                    scope: selector.parse::<ScopeSelectors>().ok()?,
                    style: StyleModifier {
                        foreground: Some(kind_colour(*kind)),
                        background: None,
                        font_style: None,
                    },
                })
            })
            .collect(),
        ..Theme::default()
    })
}

/// The kind travels through syntect as a colour whose red byte is its index.
fn kind_colour(kind: Kind) -> ThemeColor {
    let index = KINDS.iter().position(|k| *k == kind).unwrap_or(0);
    ThemeColor {
        r: index as u8,
        g: 0,
        b: 0,
        a: 0xFF,
    }
}

fn colour_kind(colour: ThemeColor) -> Kind {
    KINDS
        .get(usize::from(colour.r))
        .copied()
        .unwrap_or(Kind::Plain)
}

/// The grammar for a fence tag (`rust`, `py`, `tsx`, `sh`), case-insensitive.
pub fn syntax_for_tag(tag: &str) -> Option<&'static SyntaxReference> {
    // A fence info string may carry more after the language (`rust,ignore`,
    // `py title="x"`).
    let tag = tag
        .split(|c: char| c.is_whitespace() || c == ',' || c == '{')
        .next()?
        .trim()
        .to_ascii_lowercase();
    if tag.is_empty() {
        return None;
    }
    let tag = aliased(&tag);
    let set = syntaxes();
    set.find_syntax_by_token(tag)
        .or_else(|| set.find_syntax_by_extension(tag))
        .filter(|syntax| !is_plain_text(syntax))
}

/// The grammar for a file path, by extension or well-known file name
/// (`Makefile`, `Dockerfile`).
pub fn syntax_for_path(path: &str) -> Option<&'static SyntaxReference> {
    let name = path.trim().rsplit(['/', '\\']).next()?;
    let lower = name.to_ascii_lowercase();
    let set = syntaxes();
    if let Some(syntax) = set.find_syntax_by_extension(name) {
        return Some(syntax).filter(|s| !is_plain_text(s));
    }
    let (_, extension) = lower.rsplit_once('.').unwrap_or(("", lower.as_str()));
    syntax_for_tag(extension)
}

fn aliased(tag: &str) -> &str {
    ALIASES
        .iter()
        .find(|(from, _)| *from == tag)
        .map_or(tag, |(_, to)| to)
}

fn is_plain_text(syntax: &SyntaxReference) -> bool {
    syntax.name == "Plain Text"
}

/// `code` split into lines of tokens, without line endings. Lines past the
/// byte budget come back as one plain token each.
pub fn highlight(code: &str, syntax: &'static SyntaxReference) -> Vec<Vec<Token>> {
    if code.len() > MAX_CACHED_BYTES {
        return highlight_uncached(code, syntax);
    }
    let key = CacheKey::new(syntax, code);
    if let Some(hit) = CACHE.with(|cache| cache.borrow_mut().get(&key)) {
        return hit;
    }
    // A block still streaming grows a little on every frame; only its
    // latest text is worth keeping, so the version it grew from goes.
    let grew_from = STREAM.with(|stream| {
        let stream = stream.borrow();
        let checkpoint = stream.as_ref()?;
        let extends = checkpoint.syntax == syntax.name
            && checkpoint.whole.len() < code.len()
            && code.starts_with(checkpoint.whole.as_str());
        extends.then(|| CacheKey::new(syntax, &checkpoint.whole))
    });
    let lines = highlight_uncached(code, syntax);
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(previous) = grew_from {
            cache.remove(&previous);
        }
        cache.insert(key, code.len(), lines.clone());
    });
    lines
}

/// A cached block: its grammar and a hash of its text, with the length as
/// a guard, so the cache doesn't hold a second copy of every block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct CacheKey {
    hash: u64,
    len: usize,
}

impl CacheKey {
    fn new(syntax: &SyntaxReference, code: &str) -> Self {
        let mut hasher = DefaultHasher::new();
        syntax.name.hash(&mut hasher);
        code.hash(&mut hasher);
        Self {
            hash: hasher.finish(),
            len: code.len(),
        }
    }
}

struct CacheEntry {
    lines: Vec<Vec<Token>>,
    bytes: usize,
    last_used: u64,
}

/// Highlighted blocks bounded by their estimated memory, least recently
/// used dropped first.
struct HighlightCache {
    entries: HashMap<CacheKey, CacheEntry>,
    bytes: usize,
    budget: usize,
    clock: u64,
}

impl HighlightCache {
    fn with_budget(budget: usize) -> Self {
        Self {
            entries: HashMap::new(),
            bytes: 0,
            budget,
            clock: 0,
        }
    }

    fn get(&mut self, key: &CacheKey) -> Option<Vec<Vec<Token>>> {
        self.clock += 1;
        let entry = self.entries.get_mut(key)?;
        entry.last_used = self.clock;
        Some(entry.lines.clone())
    }

    fn insert(&mut self, key: CacheKey, code_len: usize, lines: Vec<Vec<Token>>) {
        self.remove(&key);
        let bytes = code_len + tokens_bytes(&lines);
        if bytes > self.budget {
            return;
        }
        while self.bytes + bytes > self.budget {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| *key)
            else {
                break;
            };
            self.remove(&oldest);
        }
        self.clock += 1;
        self.bytes += bytes;
        self.entries.insert(
            key,
            CacheEntry {
                lines,
                bytes,
                last_used: self.clock,
            },
        );
    }

    fn remove(&mut self, key: &CacheKey) {
        if let Some(entry) = self.entries.remove(key) {
            self.bytes -= entry.bytes;
        }
    }
}

/// Estimated heap held by highlighted lines: the token text plus the
/// vectors that hold it.
fn tokens_bytes(lines: &[Vec<Token>]) -> usize {
    let per_line = std::mem::size_of::<Vec<Token>>();
    let per_token = std::mem::size_of::<Token>();
    lines
        .iter()
        .map(|tokens| {
            per_line
                + tokens
                    .iter()
                    .map(|(text, _)| per_token + text.len())
                    .sum::<usize>()
        })
        .sum()
}

thread_local! {
    static CACHE: RefCell<HighlightCache> =
        RefCell::new(HighlightCache::with_budget(CACHE_BUDGET_BYTES));
}

fn highlight_uncached(code: &str, syntax: &SyntaxReference) -> Vec<Vec<Token>> {
    let highlighter = Highlighter::new(palette_theme());
    let complete_end = code.rfind('\n').map_or(0, |at| at + 1);
    let (mut lines, mut progress, start) = resume(code, syntax, complete_end)
        .unwrap_or_else(|| (Vec::new(), Progress::new(syntax), 0));
    for line in LinesWithEndings::from(&code[start..complete_end]) {
        lines.push(progress.line(line, &highlighter));
    }
    let checkpoint = Checkpoint {
        syntax: syntax.name.clone(),
        whole: code.to_string(),
        prefix_len: complete_end,
        lines: lines.clone(),
        progress: progress.clone(),
    };
    STREAM.with(|stream| *stream.borrow_mut() = Some(checkpoint));
    // A line still being written is highlighted from a copy of the state,
    // so the next call can resume after the last complete line.
    if complete_end < code.len() {
        lines.push(progress.line(&code[complete_end..], &highlighter));
    }
    lines
}

/// Where highlighting got to: the grammar's state and the byte budget.
#[derive(Clone)]
struct Progress {
    parse: ParseState,
    highlight: HighlightState,
    budget: usize,
    highlighting: bool,
}

impl Progress {
    fn new(syntax: &SyntaxReference) -> Self {
        let highlighter = Highlighter::new(palette_theme());
        Self {
            parse: ParseState::new(syntax),
            highlight: HighlightState::new(&highlighter, ScopeStack::new()),
            budget: MAX_HIGHLIGHT_BYTES,
            highlighting: true,
        }
    }

    /// The tokens of one line (with its line ending, if any).
    fn line(&mut self, line: &str, highlighter: &Highlighter) -> Vec<Token> {
        let text = line.trim_end_matches(['\n', '\r']);
        if text.len() > MAX_HIGHLIGHT_LINE || line.len() > self.budget {
            self.highlighting = false;
        }
        if !self.highlighting {
            return plain_line(text);
        }
        self.budget -= line.len();
        match self.parse.parse_line(line, syntaxes()) {
            Ok(ops) => {
                let pieces: Vec<_> =
                    HighlightIterator::new(&mut self.highlight, &ops, line, highlighter).collect();
                tokens_of(&pieces)
            }
            // A grammar error leaves the rest of the block plain.
            Err(_) => {
                self.highlighting = false;
                plain_line(text)
            }
        }
    }
}

/// The last block highlighted, up to its last complete line, so a block
/// that grows while an answer streams is highlighted only where it grew.
struct Checkpoint {
    syntax: String,
    /// The whole block as last highlighted, partial line included.
    whole: String,
    /// Where its last complete line ends: `lines` and `progress` cover
    /// `whole[..prefix_len]`.
    prefix_len: usize,
    lines: Vec<Vec<Token>>,
    progress: Progress,
}

thread_local! {
    static STREAM: RefCell<Option<Checkpoint>> = const { RefCell::new(None) };
}

/// The lines and state to continue from when `code` extends the last
/// block highlighted, and the byte offset to continue at.
fn resume(
    code: &str,
    syntax: &SyntaxReference,
    complete_end: usize,
) -> Option<(Vec<Vec<Token>>, Progress, usize)> {
    STREAM.with(|stream| {
        let stream = stream.borrow();
        let checkpoint = stream.as_ref()?;
        let prefix = &checkpoint.whole[..checkpoint.prefix_len];
        let extends = checkpoint.syntax == syntax.name
            && checkpoint.prefix_len <= complete_end
            && code.starts_with(prefix);
        extends.then(|| {
            (
                checkpoint.lines.clone(),
                checkpoint.progress.clone(),
                checkpoint.prefix_len,
            )
        })
    })
}

fn tokens_of(pieces: &[(syntect::highlighting::Style, &str)]) -> Vec<Token> {
    let mut tokens: Vec<Token> = Vec::new();
    for (style, text) in pieces {
        let text = text.trim_end_matches(['\n', '\r']);
        if text.is_empty() {
            continue;
        }
        let kind = colour_kind(style.foreground);
        match tokens.last_mut() {
            Some((last, last_kind)) if *last_kind == kind => last.push_str(text),
            _ => tokens.push((text.to_string(), kind)),
        }
    }
    tokens
}

fn plain_line(text: &str) -> Vec<Token> {
    if text.is_empty() {
        Vec::new()
    } else {
        vec![(text.to_string(), Kind::Plain)]
    }
}

/// The style for a kind on top of `base` (the surface's own colours).
pub fn style(kind: Kind, surface: Surface, base: Style) -> Style {
    let colour = match kind {
        Kind::Plain => return base,
        Kind::Comment => theme::MUTED,
        Kind::Keyword => theme::ACCENT_ALT,
        Kind::String | Kind::Inserted => theme::OK,
        Kind::Constant => theme::WARN,
        Kind::Function | Kind::Type | Kind::Heading => theme::ACCENT,
        Kind::Deleted | Kind::Invalid => theme::ERROR,
    };
    let styled = match surface {
        Surface::Plain => base.fg(colour),
        // Green text on a green bar (or red on red) loses its contrast;
        // such tokens keep the bar's own text colour.
        Surface::Bar if blends_into_bar(colour, base.bg) => base,
        Surface::Bar => base.fg(theme::on_bar(colour)),
    };
    match kind {
        Kind::Comment => styled.add_modifier(Modifier::ITALIC),
        Kind::Heading => styled.add_modifier(Modifier::BOLD),
        _ => styled,
    }
}

fn blends_into_bar(colour: Color, bar: Option<Color>) -> bool {
    (colour == theme::OK && theme::is_added_bar(bar))
        || (colour == theme::ERROR && theme::is_removed_bar(bar))
}

/// Highlighted lines as styled runs, or `None` when the language is unknown
/// (the caller then shows the code in its usual plain style).
pub fn styled_lines(
    code: &str,
    syntax: Option<&'static SyntaxReference>,
    surface: Surface,
    base: Style,
) -> Option<Vec<Vec<(String, Style)>>> {
    let syntax = syntax?;
    Some(
        highlight(code, syntax)
            .into_iter()
            .map(|tokens| {
                tokens
                    .into_iter()
                    .map(|(text, kind)| (text, style(kind, surface, base)))
                    .collect()
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds_of(code: &str, tag: &str) -> Vec<Vec<Token>> {
        highlight(code, syntax_for_tag(tag).expect("known language"))
    }

    fn kind_of(lines: &[Vec<Token>], needle: &str) -> Kind {
        lines
            .iter()
            .flatten()
            .find(|(text, _)| text.contains(needle))
            .map(|(_, kind)| *kind)
            .unwrap_or_else(|| panic!("{needle} not found in {lines:?}"))
    }

    #[test]
    fn rust_tokens_get_their_kinds() {
        let lines = kinds_of(
            "// note\nfn main() {\n    let s = \"hi\";\n    let n = 42;\n}\n",
            "rust",
        );
        assert_eq!(lines.len(), 5);
        assert_eq!(kind_of(&lines, "// note"), Kind::Comment);
        assert_eq!(kind_of(&lines, "fn"), Kind::Keyword);
        assert_eq!(kind_of(&lines, "main"), Kind::Function);
        assert_eq!(kind_of(&lines, "hi"), Kind::String);
        assert_eq!(kind_of(&lines, "42"), Kind::Constant);
    }

    #[test]
    fn typescript_types_and_keywords_get_their_kinds() {
        let lines = kinds_of(
            "interface Order { id: number }\nconst total: number = 42;\n",
            "ts",
        );
        assert_eq!(kind_of(&lines, "interface"), Kind::Keyword);
        assert_eq!(kind_of(&lines, "Order"), Kind::Type);
        assert_eq!(kind_of(&lines, "42"), Kind::Constant);
    }

    #[test]
    fn text_is_kept_exactly() {
        let code = "def f(x):\n    return x + 1  # add\n\n";
        let lines = kinds_of(code, "python");
        let joined: Vec<String> = lines
            .iter()
            .map(|tokens| tokens.iter().map(|(t, _)| t.as_str()).collect())
            .collect();
        assert_eq!(joined, ["def f(x):", "    return x + 1  # add", ""]);
    }

    #[test]
    fn fence_tags_and_paths_find_grammars() {
        for tag in [
            "rust", "rs", "py", "Python", "sh", "bash", "zsh", "ts", "tsx", "sql", "jsx", "mjs",
            "jsonc", "console", "pgsql",
        ] {
            assert!(syntax_for_tag(tag).is_some(), "{tag}");
        }
        for (tag, grammar) in [
            ("ts", "TypeScript"),
            ("typescript", "TypeScript"),
            ("tsx", "TypeScriptReact"),
            ("toml", "TOML"),
            ("dockerfile", "Dockerfile"),
            ("kotlin", "Kotlin"),
            ("swift", "Swift"),
            ("hcl", "Terraform"),
        ] {
            assert_eq!(syntax_for_tag(tag).map(|s| s.name.as_str()), Some(grammar));
        }
        assert!(syntax_for_tag("rust,ignore").is_some());
        assert!(syntax_for_tag("").is_none());
        assert!(syntax_for_tag("text").is_none());
        assert!(syntax_for_tag("no-such-language").is_none());
        assert_eq!(syntax_for_path("src/main.rs").unwrap().name, "Rust");
        assert_eq!(
            syntax_for_path("web/App.tsx").unwrap().name,
            "TypeScriptReact"
        );
        assert_eq!(syntax_for_path("Cargo.toml").unwrap().name, "TOML");
        assert_eq!(
            syntax_for_path("deploy/Dockerfile").unwrap().name,
            "Dockerfile"
        );
        assert_eq!(syntax_for_path("Makefile").unwrap().name, "Makefile");
        assert!(syntax_for_path("notes.unknownext").is_none());
    }

    #[test]
    fn diff_fences_colour_added_and_removed_lines() {
        let lines = kinds_of("-old\n+new\n", "diff");
        assert_eq!(kind_of(&lines, "old"), Kind::Deleted);
        assert_eq!(kind_of(&lines, "new"), Kind::Inserted);
    }

    #[test]
    fn huge_blocks_stay_plain_past_the_budget() {
        let line = "let x = 1;\n";
        let code = line.repeat(MAX_HIGHLIGHT_BYTES / line.len() + 50);
        let lines = kinds_of(&code, "rust");
        assert_eq!(lines.len(), code.lines().count());
        assert_eq!(kind_of(&lines[..1], "let"), Kind::Keyword);
        let last = lines.last().unwrap();
        assert_eq!(last, &vec![("let x = 1;".to_string(), Kind::Plain)]);
    }

    #[test]
    fn very_long_lines_are_not_highlighted() {
        let code = format!(
            "let a = 1;\nlet b = \"{}\";\nlet c = 2;\n",
            "x".repeat(5_000)
        );
        let lines = kinds_of(&code, "rust");
        assert_eq!(kind_of(&lines[..1], "let"), Kind::Keyword);
        assert_eq!(lines[2], vec![("let c = 2;".to_string(), Kind::Plain)]);
    }

    #[test]
    fn a_growing_block_highlights_the_same_as_a_whole_one() {
        let syntax = syntax_for_tag("rust").unwrap();
        let whole = "let s = \"a\nb\";\n// done\nfn main() {}\n";
        let mut grown = String::new();
        let mut streamed = Vec::new();
        for ch in whole.chars() {
            grown.push(ch);
            streamed = highlight_uncached(&grown, syntax);
            assert_eq!(
                streamed.len(),
                LinesWithEndings::from(grown.as_str()).count()
            );
        }
        STREAM.with(|stream| *stream.borrow_mut() = None);
        let fresh = highlight_uncached(whole, syntax);
        assert_eq!(streamed, fresh);
        assert_eq!(
            kind_of(&fresh[1..2], "b"),
            Kind::String,
            "the string spans lines"
        );
        assert_eq!(kind_of(&fresh[2..3], "done"), Kind::Comment);
        // A different block starts over instead of resuming.
        let other = highlight_uncached("# heading\n", syntax_for_tag("md").unwrap());
        assert_eq!(other.len(), 1);
    }

    fn cached_blocks() -> (usize, usize) {
        CACHE.with(|cache| {
            let cache = cache.borrow();
            (cache.entries.len(), cache.bytes)
        })
    }

    fn reset_cache() {
        CACHE.with(|cache| *cache.borrow_mut() = HighlightCache::with_budget(CACHE_BUDGET_BYTES));
        STREAM.with(|stream| *stream.borrow_mut() = None);
    }

    #[test]
    fn a_streamed_block_leaves_one_cache_entry() {
        reset_cache();
        let syntax = syntax_for_tag("rust").unwrap();
        let whole = "fn main() {\n    let total = 1 + 2;\n}\n".repeat(40);
        let mut grown = String::new();
        for ch in whole.chars() {
            grown.push(ch);
            highlight(&grown, syntax);
        }
        let (entries, bytes) = cached_blocks();
        assert_eq!(entries, 1, "only the latest text of the block is kept");
        assert!(bytes < 4 * whole.len() + 64 * 1024, "{bytes} bytes cached");
        assert_eq!(highlight(&whole, syntax), {
            reset_cache();
            highlight(&whole, syntax)
        });
    }

    #[test]
    fn finished_blocks_stay_cached_side_by_side() {
        reset_cache();
        let syntax = syntax_for_tag("rust").unwrap();
        let first = "let a = 1;\nlet b = 2;\n";
        let second = "let a = 1;\nfn other() {}\n";
        highlight(first, syntax);
        highlight(second, syntax);
        highlight(first, syntax);
        assert_eq!(cached_blocks().0, 2, "blocks that share lines both stay");
    }

    #[test]
    fn the_cache_is_bounded_by_bytes_dropping_the_least_recent() {
        let syntax = syntax_for_tag("rust").unwrap();
        let lines = vec![vec![("x".repeat(100), Kind::Plain)]];
        let one = 10 + tokens_bytes(&lines);
        let mut cache = HighlightCache::with_budget(3 * one);
        let keys: Vec<CacheKey> = (0..4)
            .map(|n| CacheKey::new(syntax, &format!("block {n}")))
            .collect();
        for key in &keys[..3] {
            cache.insert(*key, 10, lines.clone());
        }
        assert!(cache.get(&keys[0]).is_some(), "touch the oldest");
        cache.insert(keys[3], 10, lines.clone());
        assert_eq!(cache.bytes, 3 * one);
        assert!(cache.get(&keys[1]).is_none(), "least recently used dropped");
        for kept in [keys[0], keys[2], keys[3]] {
            assert!(cache.get(&kept).is_some());
        }
        let huge = vec![vec![("x".repeat(10 * one), Kind::Plain)]];
        cache.insert(CacheKey::new(syntax, "huge"), 10, huge);
        assert_eq!(cache.entries.len(), 3, "a block over the budget isn't kept");
    }

    #[test]
    fn colours_are_ansi_and_brighter_on_bars() {
        let base = Style::default();
        assert_eq!(
            style(Kind::String, Surface::Plain, base).fg,
            Some(theme::OK)
        );
        assert_eq!(
            style(Kind::String, Surface::Bar, theme::diff_removed()).fg,
            Some(Color::LightGreen)
        );
        assert_eq!(
            style(Kind::Comment, Surface::Bar, theme::diff_added()).fg,
            Some(Color::Gray)
        );
        assert_eq!(
            style(Kind::Plain, Surface::Bar, theme::diff_added()),
            theme::diff_added()
        );
        let on_bar = style(Kind::Keyword, Surface::Bar, theme::diff_removed());
        assert_eq!(on_bar.bg, theme::diff_removed().bg, "bar colour kept");
        for kind in KINDS {
            for surface in [Surface::Plain, Surface::Bar] {
                assert!(
                    !matches!(style(kind, surface, base).fg, Some(Color::Rgb(..))),
                    "{kind:?} uses a 24-bit colour"
                );
            }
        }
    }

    #[test]
    fn same_hue_tokens_keep_the_bars_text_colour() {
        for bar in [theme::diff_added(), theme::diff_added_word()] {
            for kind in [Kind::String, Kind::Inserted] {
                assert_eq!(style(kind, Surface::Bar, bar).fg, bar.fg, "{kind:?}");
            }
            assert_eq!(
                style(Kind::Deleted, Surface::Bar, bar).fg,
                Some(Color::LightRed)
            );
        }
        for bar in [theme::diff_removed(), theme::diff_removed_word()] {
            for kind in [Kind::Deleted, Kind::Invalid] {
                assert_eq!(style(kind, Surface::Bar, bar).fg, bar.fg, "{kind:?}");
            }
        }
        let word = style(Kind::String, Surface::Bar, theme::diff_added_word());
        assert!(word.add_modifier.contains(Modifier::BOLD), "emphasis kept");
    }

    #[test]
    fn unknown_languages_are_left_to_the_caller() {
        assert!(styled_lines("x", None, Surface::Plain, Style::default()).is_none());
    }
}
