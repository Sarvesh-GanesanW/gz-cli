//! What a command line runs, as deny and ask rules see it: every simple
//! command in it (across `;`, `&&`, pipes, substitutions, `sh -c '…'`,
//! `eval`, `find -exec` and wrappers like `sudo -u root` or `timeout 5`),
//! with quotes taken off and the program's folder dropped. Constructs whose
//! program can't be known before the shell runs them (`$CMD x`, `eval
//! "$X"`, `curl … | sh`) make the line opaque, so a rule that can't see
//! inside asks instead of letting it through.
//!
//! This reads POSIX shell syntax and errs towards finding more commands
//! than run (a heredoc's lines are read as commands), never fewer.

use std::iter::Peekable;
use std::str::Chars;

/// How deep `sh -c`, `eval` and substitutions may nest before the line
/// counts as opaque.
const MAX_DEPTH: usize = 8;

/// Shells that run a `-c` string, or read commands from stdin without an
/// operand.
const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh", "mksh", "ash", "fish"];

/// Shell keywords that run the command after them.
const KEYWORDS: &[&str] = &["if", "then", "else", "elif", "do", "while", "until", "!"];

/// What one command line runs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Analysis {
    /// Each simple command, once per wrapper layer: `sudo nice rm x` gives
    /// `sudo nice rm x`, `nice rm x` and `rm x`.
    pub forms: Vec<String>,
    /// Some command's program can't be known without running the line.
    pub opaque: bool,
}

/// Every command `line` runs, as far as reading it can tell.
pub fn analyse(line: &str) -> Analysis {
    let mut analysis = Analysis::default();
    analyse_into(line, 0, &mut analysis);
    analysis
}

fn analyse_into(line: &str, depth: usize, out: &mut Analysis) {
    if depth > MAX_DEPTH {
        out.opaque = true;
        return;
    }
    let lexed = lex(line);
    for nested in &lexed.substitutions {
        analyse_into(nested, depth + 1, out);
    }
    for command in &lexed.commands {
        unwrap_command(command, depth, out);
    }
}

/// One word with quotes and escapes taken off.
#[derive(Debug, Clone, Default)]
struct Word {
    text: String,
    /// It holds an expansion or an unquoted glob, so its final text is
    /// only known when the shell runs it.
    expands: bool,
}

#[derive(Debug, Default)]
struct Lexed {
    commands: Vec<Vec<Word>>,
    /// The bodies of `$(…)` and backquotes, which run as commands too.
    substitutions: Vec<String>,
}

/// Characters that end a simple command (`{` and `}` do only as words of
/// their own, so `find -exec rm {}` keeps its `{}`).
fn ends_command(c: char) -> bool {
    matches!(c, ';' | '&' | '|' | '\n' | '\r' | '(' | ')')
}

struct Lexer<'a> {
    chars: Peekable<Chars<'a>>,
    out: Lexed,
    command: Vec<Word>,
    word: Option<Word>,
    /// The next word is a redirection's target, not an argument.
    redirect_target: bool,
}

fn lex(line: &str) -> Lexed {
    let mut lexer = Lexer {
        chars: line.chars().peekable(),
        out: Lexed::default(),
        command: Vec::new(),
        word: None,
        redirect_target: false,
    };
    lexer.run();
    lexer.out
}

impl Lexer<'_> {
    fn run(&mut self) {
        while let Some(c) = self.chars.next() {
            match c {
                '#' if self.word.is_none() => self.skip_comment(),
                '{' if self.word.is_none()
                    && self.chars.peek().is_none_or(|c| c.is_whitespace()) =>
                {
                    self.end_command()
                }
                '}' if self.word.is_none() => self.end_command(),
                '{' | '}' => self.word().text.push(c),
                c if ends_command(c) => {
                    self.end_word();
                    self.end_command();
                }
                c if c.is_whitespace() => self.end_word(),
                '<' | '>' => self.redirect(),
                '\'' => {
                    let quoted: String = self.chars.by_ref().take_while(|&c| c != '\'').collect();
                    self.word().text.push_str(&quoted);
                }
                '"' => self.double_quoted(),
                '\\' => match self.chars.next() {
                    Some('\n') | None => {}
                    Some(escaped) => self.word().text.push(escaped),
                },
                '`' => self.backquoted(),
                '$' => self.dollar(),
                '*' | '?' | '[' => {
                    let word = self.word();
                    word.text.push(c);
                    word.expands = true;
                }
                c => self.word().text.push(c),
            }
        }
        self.end_word();
        self.end_command();
    }

    fn word(&mut self) -> &mut Word {
        self.word.get_or_insert_with(Word::default)
    }

    fn end_word(&mut self) {
        let Some(word) = self.word.take() else {
            return;
        };
        if std::mem::take(&mut self.redirect_target) {
            return;
        }
        self.command.push(word);
    }

    fn end_command(&mut self) {
        self.redirect_target = false;
        if !self.command.is_empty() {
            self.out.commands.push(std::mem::take(&mut self.command));
        }
    }

    fn skip_comment(&mut self) {
        while self.chars.next_if(|&c| c != '\n').is_some() {}
    }

    /// `>`, `>>`, `2>&1`, `<`, `<<EOF`: the target is not an argument, and
    /// a file descriptor number before it isn't either.
    fn redirect(&mut self) {
        let is_fd = self
            .word
            .as_ref()
            .is_some_and(|w| !w.text.is_empty() && w.text.chars().all(|c| c.is_ascii_digit()));
        if is_fd {
            self.word = None;
        } else {
            self.end_word();
        }
        while self
            .chars
            .next_if(|&c| matches!(c, '>' | '<' | '&' | '|' | '-'))
            .is_some()
        {}
        self.redirect_target = true;
    }

    fn double_quoted(&mut self) {
        self.word();
        while let Some(c) = self.chars.next() {
            match c {
                '"' => return,
                '\\' => match self.chars.next() {
                    Some(escaped @ ('$' | '`' | '"' | '\\')) => self.word().text.push(escaped),
                    Some('\n') | None => {}
                    Some(other) => {
                        let word = self.word();
                        word.text.push('\\');
                        word.text.push(other);
                    }
                },
                '`' => self.backquoted(),
                '$' => self.dollar(),
                c => self.word().text.push(c),
            }
        }
    }

    fn backquoted(&mut self) {
        let mut body = String::new();
        while let Some(c) = self.chars.next() {
            match c {
                '`' => break,
                '\\' => body.extend(self.chars.next()),
                c => body.push(c),
            }
        }
        self.out.substitutions.push(body);
        self.word().expands = true;
    }

    /// `$(…)` runs a command; `$((…))`, `${…}` and `$NAME` only expand.
    fn dollar(&mut self) {
        match self.chars.peek() {
            Some('(') => {
                self.chars.next();
                let body = take_until_close(&mut self.chars);
                match body.strip_prefix('(') {
                    Some(arithmetic) => self
                        .out
                        .substitutions
                        .extend(inner_substitutions(arithmetic)),
                    None => self.out.substitutions.push(body),
                }
            }
            Some('{') => {
                self.chars.next();
                while self.chars.next_if(|&c| c != '}').is_some() {}
                self.chars.next();
            }
            _ => {}
        }
        let word = self.word();
        word.text.push('$');
        word.expands = true;
    }
}

/// The text up to the `)` that closes an already-read `(`, skipping
/// parentheses inside quotes.
fn take_until_close(chars: &mut Peekable<Chars>) -> String {
    let mut body = String::new();
    let mut depth = 1usize;
    let mut quote: Option<char> = None;
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"') => quote = Some(c),
            (None, '\\') => {
                body.push(c);
                body.extend(chars.next());
                continue;
            }
            (None, '(') => depth += 1,
            (None, ')') => {
                depth -= 1;
                if depth == 0 {
                    return body;
                }
            }
            _ => {}
        }
        body.push(c);
    }
    body
}

/// The command substitutions inside an arithmetic expansion.
fn inner_substitutions(text: &str) -> Vec<String> {
    lex(text).substitutions
}

/// How a wrapper's options are read: which take a value, and how many
/// plain words (a duration, a priority) come before the command.
struct WrapperOptions {
    /// Short option letters that take a value (`-u root`, `-uroot`).
    short_values: &'static str,
    /// Long options that take a value (`--user root`, `--user=root`).
    long_values: &'static [&'static str],
    /// Words before the command that aren't options.
    operands: usize,
}

/// Programs that run the command after them, and how their options read.
fn wrapper(program: &str) -> Option<WrapperOptions> {
    let options = |short_values, long_values, operands| WrapperOptions {
        short_values,
        long_values,
        operands,
    };
    Some(match program {
        "sudo" => options(
            "ughpCUrtDRT",
            &[
                "--user",
                "--group",
                "--host",
                "--prompt",
                "--close-from",
                "--other-user",
                "--role",
                "--type",
                "--chdir",
                "--chroot",
                "--command-timeout",
            ],
            0,
        ),
        "doas" => options("uC", &[], 0),
        "env" => options("uCP", &["--unset", "--chdir"], 0),
        "nice" => options("n", &["--adjustment"], 0),
        "ionice" => options("cnpP", &["--class", "--classdata", "--pid", "--pgid"], 0),
        "chrt" => options(
            "TPD",
            &["--sched-runtime", "--sched-period", "--sched-deadline"],
            1,
        ),
        "taskset" => options("", &[], 1),
        "timeout" | "gtimeout" => options("sk", &["--signal", "--kill-after"], 1),
        "stdbuf" | "gstdbuf" => options("ioe", &["--input", "--output", "--error"], 0),
        "xargs" | "gxargs" => options(
            "IiLlnPsdEa",
            &[
                "--replace",
                "--max-lines",
                "--max-args",
                "--max-procs",
                "--max-chars",
                "--delimiter",
                "--eof",
                "--arg-file",
            ],
            0,
        ),
        "time" | "gtime" => options("fo", &["--format", "--output"], 0),
        "exec" => options("a", &[], 0),
        "caffeinate" => options("tw", &[], 0),
        "nohup" | "command" | "builtin" | "setsid" | "unbuffer" | "noglob" | "nocorrect" => {
            options("", &[], 0)
        }
        _ => return None,
    })
}

/// Records each wrapper layer of one simple command, following `sh -c`,
/// `eval`, `env -S`, `watch` and `find -exec` into the commands they run.
fn unwrap_command(words: &[Word], depth: usize, out: &mut Analysis) {
    let mut i = 0;
    loop {
        i += words[i..]
            .iter()
            .take_while(|w| is_assignment(&w.text))
            .count();
        let Some(word) = words.get(i) else {
            return;
        };
        if word.expands || word.text.is_empty() {
            out.opaque = true;
            return;
        }
        let program = base_name(&word.text);
        out.forms.push(form(program, &words[i + 1..]));
        let rest = &words[i + 1..];
        if KEYWORDS.contains(&program) {
            i += 1;
            continue;
        }
        if SHELLS.contains(&program) {
            return shell(rest, depth, out);
        }
        match program {
            "eval" => return run_text(&joined(rest), depth, out),
            "watch" => return run_text(&joined(after_options(rest, "nd")), depth, out),
            "find" | "gfind" => return find_exec(rest, depth, out),
            "command"
                if rest
                    .first()
                    .is_some_and(|w| matches!(&*w.text, "-v" | "-V")) =>
            {
                return
            }
            "env" => {
                if let Some(split) = env_split_string(rest) {
                    return run_text(&split, depth, out);
                }
            }
            _ => {}
        }
        let Some(options) = wrapper(program) else {
            return;
        };
        let after = after_wrapper(rest, &options);
        i = words.len() - after.len();
    }
}

/// `sh -c 'script'` runs the script; a shell with no operand reads
/// commands from stdin (`curl … | sh`), which can't be read here, and one
/// with a script file runs what the file says, like any program.
fn shell(rest: &[Word], depth: usize, out: &mut Analysis) {
    let mut runs_string = false;
    let mut words = rest.iter();
    while let Some(word) = words.next() {
        let text = word.text.as_str();
        let operand = match text {
            "--" => words.next(),
            "-" => None,
            "--rcfile" | "--init-file" => {
                words.next();
                continue;
            }
            long if long.starts_with("--") => continue,
            option if option.len() > 1 && option.starts_with(['-', '+']) => {
                let letters = &option[1..];
                runs_string |= option.starts_with('-') && letters.contains('c');
                // `-o name` / `+O name` set a named option.
                if letters.ends_with(['o', 'O']) {
                    words.next();
                }
                continue;
            }
            _ => Some(word),
        };
        match operand {
            Some(script) if runs_string => run_text_word(script, depth, out),
            Some(_) => {}
            None => out.opaque = true,
        }
        return;
    }
    out.opaque = true;
}

fn run_text_word(word: &Word, depth: usize, out: &mut Analysis) {
    if word.expands {
        out.opaque = true;
    }
    analyse_into(&word.text, depth + 1, out);
}

fn run_text(text: &str, depth: usize, out: &mut Analysis) {
    analyse_into(text, depth + 1, out);
}

/// `find … -exec cmd {} ;` (and `-execdir`, `-ok`, `-okdir`) runs `cmd`.
fn find_exec(rest: &[Word], depth: usize, out: &mut Analysis) {
    let mut words = rest.iter();
    while let Some(word) = words.next() {
        if !matches!(&*word.text, "-exec" | "-execdir" | "-ok" | "-okdir") {
            continue;
        }
        let command: Vec<Word> = words
            .by_ref()
            .take_while(|w| !matches!(&*w.text, ";" | "+"))
            .cloned()
            .collect();
        if !command.is_empty() {
            unwrap_command(&command, depth + 1, out);
        }
    }
}

/// The string of `env -S 'cmd args'` / `env --split-string=…`.
fn env_split_string(rest: &[Word]) -> Option<String> {
    let mut words = rest.iter();
    while let Some(word) = words.next() {
        let text = word.text.as_str();
        if let Some(value) = text.strip_prefix("--split-string=") {
            return Some(value.to_string());
        }
        if text == "--split-string" || text == "-S" {
            return words.next().map(|w| w.text.clone());
        }
        if let Some(value) = text.strip_prefix("-S") {
            return Some(value.to_string());
        }
        if !text.starts_with('-') {
            return None;
        }
    }
    None
}

/// The words after a wrapper's options and operands: the command it runs.
fn after_wrapper<'w>(rest: &'w [Word], options: &WrapperOptions) -> &'w [Word] {
    let mut rest = after_long_options(rest, options);
    for _ in 0..options.operands {
        rest = rest.get(1..).unwrap_or_default();
        rest = after_long_options(rest, options);
    }
    rest
}

fn after_long_options<'w>(rest: &'w [Word], options: &WrapperOptions) -> &'w [Word] {
    let mut i = 0;
    while let Some(word) = rest.get(i) {
        let text = word.text.as_str();
        if text == "--" {
            return rest.get(i + 1..).unwrap_or_default();
        }
        if let Some(long) = text.strip_prefix("--") {
            let takes_value = !long.contains('=')
                && options
                    .long_values
                    .iter()
                    .any(|name| name.trim_start_matches("--") == long);
            i += if takes_value { 2 } else { 1 };
            continue;
        }
        match text.strip_prefix('-').filter(|letters| !letters.is_empty()) {
            Some(letters) => i += short_option_width(letters, options.short_values),
            None => break,
        }
    }
    rest.get(i..).unwrap_or_default()
}

/// How many words a short option cluster takes: two when its last letter
/// takes a value that isn't attached.
fn short_option_width(letters: &str, short_values: &str) -> usize {
    let value_at = letters.find(|c| short_values.contains(c));
    match value_at {
        Some(at) if at + 1 == letters.len() => 2,
        _ => 1,
    }
}

/// The words after options whose letters in `short_values` take a value.
fn after_options<'w>(rest: &'w [Word], short_values: &'static str) -> &'w [Word] {
    after_long_options(
        rest,
        &WrapperOptions {
            short_values,
            long_values: &[],
            operands: 0,
        },
    )
}

fn joined(words: &[Word]) -> String {
    words
        .iter()
        .map(|w| w.text.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

/// `program args…` with single spaces.
fn form(program: &str, args: &[Word]) -> String {
    std::iter::once(program)
        .chain(args.iter().map(|w| w.text.as_str()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// `NAME=value` before a command.
fn is_assignment(word: &str) -> bool {
    match word.split_once('=') {
        Some((name, _)) => {
            !name.is_empty()
                && !name.starts_with(|c: char| c.is_ascii_digit())
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        }
        None => false,
    }
}

pub fn base_name(word: &str) -> &str {
    word.rsplit(['/', '\\']).next().unwrap_or(word)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn forms(line: &str) -> Vec<String> {
        analyse(line).forms
    }

    fn runs(line: &str, command: &str) -> bool {
        forms(line).iter().any(|f| f == command)
    }

    #[test]
    fn chains_pipes_and_substitutions_are_split_into_commands() {
        assert_eq!(forms("ls -a && rm x | wc"), ["ls -a", "rm x", "wc"]);
        assert!(runs("echo \"$(rm x)\"", "rm x"));
        assert!(runs("echo `rm x`", "rm x"));
        assert!(runs("echo $(( $(rm x) + 1 ))", "rm x"));
        assert!(runs("(cd a; rm x)", "rm x"));
        assert!(runs("f() { rm x; }; f", "rm x"));
    }

    #[test]
    fn quotes_escapes_and_paths_come_off_the_program() {
        for line in [
            "\"rm\" x",
            "'rm' x",
            "r''m x",
            "\\rm x",
            "/bin/rm x",
            "./rm x",
        ] {
            assert!(runs(line, "rm x"), "{line}: {:?}", forms(line));
        }
        assert_eq!(
            forms("git commit -m \"rm old; ls\""),
            ["git commit -m rm old; ls"]
        );
    }

    #[test]
    fn wrappers_and_their_option_values_are_seen_through() {
        for line in [
            "sudo rm x",
            "sudo -u root rm x",
            "sudo --user root rm x",
            "sudo --user=root -E rm x",
            "nice -n 5 rm x",
            "nice -10 rm x",
            "timeout 5 rm x",
            "timeout -s KILL 5s rm x",
            "env -u HOME FOO=1 rm x",
            "nohup rm x",
            "stdbuf -oL rm x",
            "xargs -I {} rm x",
            "ionice -c 3 rm x",
            "chrt 10 rm x",
            "exec -a name rm x",
            "command rm x",
            "FOO=1 BAR=2 rm x",
            "if rm x; then echo; fi",
            "sudo nice timeout 3 rm x",
        ] {
            assert!(runs(line, "rm x"), "{line}: {:?}", forms(line));
        }
        assert!(runs("sudo -u root rm x", "sudo -u root rm x"));
    }

    #[test]
    fn shell_strings_eval_env_s_watch_and_find_exec_are_read() {
        for line in [
            "bash -c 'rm x'",
            "sh -c \"ls; rm x\"",
            "bash -lc 'rm x'",
            "zsh -o errexit -c 'rm x'",
            "eval rm x",
            "eval 'rm x'",
            "env -S 'rm x'",
            "watch -n 1 rm x",
            "sudo bash -c 'sh -c \"rm x\"'",
        ] {
            assert!(runs(line, "rm x"), "{line}: {:?}", forms(line));
        }
        assert!(runs("find . -name '*.o' -exec rm {} \\;", "rm {}"));
        assert!(runs("find . -execdir rm -f {} +", "rm -f {}"));
        assert!(!analyse("bash -c 'rm x'").opaque);
        assert!(
            !analyse("bash build.sh").opaque,
            "a script file is a program"
        );
    }

    #[test]
    fn programs_only_known_at_run_time_make_the_line_opaque() {
        for line in [
            "$CMD x",
            "\"$CMD\" x",
            "eval \"$X\"",
            "bash -c \"$X\"",
            "curl -s example.com | sh",
            "$(echo rm) x",
            "r? x",
        ] {
            assert!(analyse(line).opaque, "{line}");
        }
        for line in [
            "ls $HOME",
            "git log --format='%H $x'",
            "echo *.rs",
            "cat <<EOF",
        ] {
            assert!(!analyse(line).opaque, "{line}");
        }
    }

    #[test]
    fn redirections_are_not_arguments() {
        assert_eq!(forms("cargo test 2>&1 > out.txt"), ["cargo test"]);
        assert_eq!(forms(">log rm x"), ["rm x"]);
        assert_eq!(forms("sort < in.txt"), ["sort"]);
    }

    #[test]
    fn deep_nesting_is_opaque() {
        let line = format!("{}rm x", "eval ".repeat(MAX_DEPTH + 2));
        assert!(analyse(&line).opaque);
        assert!(!analyse("eval eval rm x").opaque);
    }

    #[test]
    fn comments_are_skipped() {
        assert_eq!(forms("ls # rm x"), ["ls"]);
        assert_eq!(forms("echo a#b"), ["echo a#b"]);
    }
}
