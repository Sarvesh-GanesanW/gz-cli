//! Reads a gz command line against the clap definitions the way clap does:
//! which subcommand each word selects, and which argument each value fills.
//! Name resolution uses it to find the ids to look up, and completion uses
//! it to tell what the word under the cursor is.

use clap::{Arg, Command};

/// One value on the command line and the argument it fills.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filled {
    /// Index of the word in `words`.
    pub word: usize,
    /// The argument's id, e.g. `project`.
    pub arg: String,
    pub value: String,
    /// What comes before the value in the same word: `--project=` or `-o`.
    pub prefix: String,
    /// Which of the argument's values this is (0 for the first).
    pub nth: usize,
}

/// The subcommand chain a command line selects and the values it fills.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Walk {
    /// Subcommand names, e.g. `["jobs", "run"]`.
    pub path: Vec<String>,
    pub filled: Vec<Filled>,
    /// An option that is still waiting for its value at the end of the words.
    pub open_option: Option<String>,
    /// Positional values given to the last subcommand.
    pub positionals: usize,
    /// The words after a `--`.
    pub past_separator: bool,
}

impl Walk {
    /// `jobs run`, the key the argument roles use.
    pub fn key(&self) -> String {
        self.path.join(" ")
    }

    /// The values given to `arg`, in order.
    pub fn values_of(&self, arg: &str) -> Vec<&Filled> {
        self.filled.iter().filter(|f| f.arg == arg).collect()
    }

    pub fn first_value(&self, arg: &str) -> Option<&str> {
        self.filled
            .iter()
            .find(|f| f.arg == arg)
            .map(|f| f.value.as_str())
    }
}

/// The built command tree (global flags copied into every subcommand).
pub fn command_tree() -> Command {
    use clap::CommandFactory;
    let mut command = crate::cli::Cli::command();
    command.build();
    command
}

/// Walks `words` (without the program name) through `root`.
pub fn walk(root: &Command, words: &[String]) -> Walk {
    let mut state = Walk::default();
    let mut command = root;
    let mut index = 0;
    while index < words.len() {
        let word = &words[index];
        index += 1;
        if state.past_separator {
            fill_positional(command, &mut state, index - 1, word);
            continue;
        }
        if word == "--" {
            state.past_separator = true;
            continue;
        }
        if let Some(long) = word.strip_prefix("--").filter(|w| !w.is_empty()) {
            let (name, attached) = match long.split_once('=') {
                Some((name, value)) => (name, Some(value)),
                None => (long, None),
            };
            let Some(arg) = find_long(command, name) else {
                continue;
            };
            if !takes_value(arg) {
                continue;
            }
            let id = arg.get_id().to_string();
            match attached {
                Some(value) => state.fill(&id, index - 1, value, &format!("--{name}=")),
                None if index < words.len() => {
                    state.fill(&id, index, &words[index], "");
                    index += 1;
                }
                None => state.open_option = Some(id),
            }
            continue;
        }
        if let Some(shorts) = short_cluster(word) {
            for (at, letter) in shorts.char_indices() {
                let Some(arg) = find_short(command, letter) else {
                    break;
                };
                if !takes_value(arg) {
                    continue;
                }
                let id = arg.get_id().to_string();
                let rest = &shorts[at + letter.len_utf8()..];
                let rest = rest.strip_prefix('=').unwrap_or(rest);
                if !rest.is_empty() {
                    let prefix = &word[..word.len() - rest.len()];
                    state.fill(&id, index - 1, rest, prefix);
                } else if index < words.len() {
                    state.fill(&id, index, &words[index], "");
                    index += 1;
                } else {
                    state.open_option = Some(id);
                }
                break;
            }
            continue;
        }
        if state.positionals == 0 {
            if let Some(sub) = find_subcommand(command, word) {
                command = sub;
                state.path.push(sub.get_name().to_string());
                continue;
            }
        }
        fill_positional(command, &mut state, index - 1, word);
    }
    state
}

impl Walk {
    fn fill(&mut self, arg: &str, word: usize, value: &str, prefix: &str) {
        let nth = self.filled.iter().filter(|f| f.arg == arg).count();
        self.filled.push(Filled {
            word,
            arg: arg.to_string(),
            value: value.to_string(),
            prefix: prefix.to_string(),
            nth,
        });
    }
}

/// `-q`, `-ojson`: a word of short flags, but not a negative number.
fn short_cluster(word: &str) -> Option<&str> {
    let rest = word.strip_prefix('-')?;
    let first = rest.chars().next()?;
    (first != '-' && !first.is_ascii_digit()).then_some(rest)
}

fn fill_positional(command: &Command, state: &mut Walk, word: usize, value: &str) {
    if let Some(arg) = positional_at(command, state.positionals) {
        let id = arg.get_id().to_string();
        state.fill(&id, word, value, "");
    }
    state.positionals += 1;
}

/// The positional argument that the `n`th positional value (0-based) fills.
pub fn positional_at(command: &Command, n: usize) -> Option<&Arg> {
    let mut seen: usize = 0;
    for arg in positionals(command) {
        let most = arg
            .get_num_args()
            .map(|range| range.max_values())
            .unwrap_or(1);
        if n < seen.saturating_add(most) {
            return Some(arg);
        }
        seen = seen.saturating_add(most);
    }
    None
}

/// The command's positional arguments, in order.
pub fn positionals(command: &Command) -> Vec<&Arg> {
    let mut list: Vec<&Arg> = command
        .get_arguments()
        .filter(|arg| arg.is_positional())
        .collect();
    list.sort_by_key(|arg| arg.get_index().unwrap_or(usize::MAX));
    list
}

pub fn takes_value(arg: &Arg) -> bool {
    arg.get_num_args()
        .map(|range| range.takes_values())
        .unwrap_or(false)
}

fn find_long<'c>(command: &'c Command, name: &str) -> Option<&'c Arg> {
    command.get_arguments().find(|arg| {
        arg.get_long() == Some(name)
            || arg
                .get_all_aliases()
                .is_some_and(|aliases| aliases.contains(&name))
    })
}

fn find_short(command: &Command, letter: char) -> Option<&Arg> {
    command.get_arguments().find(|arg| {
        arg.get_short() == Some(letter)
            || arg
                .get_all_short_aliases()
                .is_some_and(|aliases| aliases.contains(&letter))
    })
}

pub fn find_subcommand<'c>(command: &'c Command, word: &str) -> Option<&'c Command> {
    command
        .get_subcommands()
        .find(|sub| sub.get_name() == word || sub.get_all_aliases().any(|alias| alias == word))
}

/// The command a walk ended in.
pub fn leaf<'c>(root: &'c Command, path: &[String]) -> &'c Command {
    path.iter().fold(root, |command, name| {
        find_subcommand(command, name).unwrap_or(command)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(line: &str) -> Vec<String> {
        line.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn finds_subcommands_positionals_and_option_values() {
        let tree = command_tree();
        let walk = walk(
            &tree,
            &words("--profile stage jobs run Sales DQC --compute=small --rte img -q"),
        );
        assert_eq!(walk.key(), "jobs run");
        let found: Vec<(&str, &str, usize, &str)> = walk
            .filled
            .iter()
            .map(|f| (f.arg.as_str(), f.value.as_str(), f.word, f.prefix.as_str()))
            .collect();
        assert_eq!(
            found,
            vec![
                ("profile", "stage", 1, ""),
                ("project", "Sales", 4, ""),
                ("job", "DQC", 5, ""),
                ("compute", "small", 6, "--compute="),
                ("rte", "img", 8, ""),
            ]
        );
        assert_eq!(walk.open_option, None);
    }

    #[test]
    fn short_options_take_attached_or_next_values() {
        let tree = command_tree();
        let walk = walk(&tree, &words("-ojson projects get Sales -o"));
        assert_eq!(walk.filled[0].arg, "output");
        assert_eq!(walk.filled[0].value, "json");
        assert_eq!(walk.filled[0].prefix, "-o");
        assert_eq!(walk.first_value("id"), Some("Sales"));
        assert_eq!(walk.open_option.as_deref(), Some("output"));
    }

    #[test]
    fn a_multi_value_positional_takes_every_value() {
        let tree = command_tree();
        let walk = walk(&tree, &words("projects job-runs Sales DQC"));
        let values: Vec<(usize, &str)> = walk
            .values_of("ids")
            .iter()
            .map(|f| (f.nth, f.value.as_str()))
            .collect();
        assert_eq!(values, vec![(0, "Sales"), (1, "DQC")]);
        assert_eq!(walk.positionals, 2);
    }
}
