//! What the word under the cursor is, read from the words before it, and
//! the candidates that need no network: subcommands, flags and fixed values.

use clap::{Arg, Command};

use crate::resolve::roles::{role_of, Role};
use crate::resolve::walk::{leaf, positional_at, takes_value, walk, Walk};

/// One completion: the word to insert and a short description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub value: String,
    pub about: String,
}

impl Item {
    pub fn new(value: impl Into<String>, about: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            about: about.into(),
        }
    }
}

/// The word under the cursor.
#[derive(Debug)]
pub enum Target<'c> {
    /// A subcommand name (and, when the command takes one, a positional value).
    Subcommand(&'c Command, Option<&'c Arg>),
    Flag(&'c Command),
    /// A value of `arg`; `prefix` is typed in front of it in the same word
    /// (`--project=`).
    Value {
        arg: &'c Arg,
        prefix: String,
    },
    Nothing,
}

/// Reads `before` (the words left of the cursor, program name left out)
/// and the `current` word.
pub fn target<'c>(root: &'c Command, before: &[String], current: &str) -> (Walk, Target<'c>) {
    let walked = walk(root, before);
    let command = leaf(root, &walked.path);
    let target = if let Some(open) = &walked.open_option {
        command
            .get_arguments()
            .find(|arg| arg.get_id() == open)
            .map(|arg| Target::Value {
                arg,
                prefix: String::new(),
            })
            .unwrap_or(Target::Nothing)
    } else if walked.past_separator {
        positional(command, walked.positionals)
    } else if let Some((flag, _)) = current.strip_prefix("--").and_then(|w| w.split_once('=')) {
        command
            .get_arguments()
            .find(|arg| arg.get_long() == Some(flag) && takes_value(arg))
            .map(|arg| Target::Value {
                arg,
                prefix: format!("--{flag}="),
            })
            .unwrap_or(Target::Nothing)
    } else if current.starts_with('-') {
        Target::Flag(command)
    } else if walked.positionals == 0 && command.has_subcommands() {
        Target::Subcommand(command, positional_at(command, 0))
    } else {
        positional(command, walked.positionals)
    };
    (walked, target)
}

fn positional(command: &Command, n: usize) -> Target<'_> {
    positional_at(command, n)
        .map(|arg| Target::Value {
            arg,
            prefix: String::new(),
        })
        .unwrap_or(Target::Nothing)
}

pub fn subcommands(command: &Command) -> Vec<Item> {
    command
        .get_subcommands()
        .filter(|sub| !sub.is_hide_set() && sub.get_name() != "help")
        .map(|sub| Item::new(sub.get_name(), about(sub.get_about())))
        .collect()
}

pub fn flags(command: &Command) -> Vec<Item> {
    command
        .get_arguments()
        .filter(|arg| !arg.is_positional() && !arg.is_hide_set())
        .filter_map(|arg| {
            let long = arg.get_long()?;
            Some(Item::new(format!("--{long}"), about(arg.get_help())))
        })
        .chain([Item::new("--help", "Print help")])
        .collect()
}

/// An argument's fixed choices (value enums, `value_parser = [..]`).
pub fn choices(arg: &Arg) -> Vec<Item> {
    arg.get_possible_values()
        .iter()
        .filter(|value| !value.is_hide_set())
        .map(|value| Item::new(value.get_name(), about(value.get_help())))
        .collect()
}

/// What completion knows `arg` to name.
pub fn role(walked: &Walk, root: &Command, arg: &Arg) -> Option<Role> {
    let command = leaf(root, &walked.path);
    role_of(&walked.key(), command, arg)
}

fn about(text: Option<&clap::builder::StyledStr>) -> String {
    text.map(|t| t.to_string())
        .unwrap_or_default()
        .lines()
        .next()
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolve::walk::command_tree;

    fn words(line: &str) -> Vec<String> {
        line.split_whitespace().map(String::from).collect()
    }

    fn values_of(items: &[Item]) -> Vec<&str> {
        items.iter().map(|i| i.value.as_str()).collect()
    }

    #[test]
    fn completes_subcommands_then_values() {
        let tree = command_tree();
        let (_, found) = target(&tree, &words("jobs"), "r");
        let Target::Subcommand(command, None) = found else {
            panic!("{found:?}")
        };
        let names = subcommands(command);
        assert!(values_of(&names).contains(&"run"));
        let (walked, found) = target(&tree, &words("jobs run"), "");
        let Target::Value { arg, .. } = found else {
            panic!("{found:?}")
        };
        assert_eq!(arg.get_id(), "project");
        assert_eq!(walked.key(), "jobs run");
    }

    #[test]
    fn completes_option_values_attached_or_not() {
        let tree = command_tree();
        let (_, found) = target(&tree, &words("jobs run 1 2 --compute"), "");
        assert!(matches!(found, Target::Value { arg, .. } if arg.get_id() == "compute"));
        let (_, found) = target(&tree, &words("jobs run 1 2"), "--compute=sm");
        assert!(
            matches!(found, Target::Value { arg, prefix, .. } if arg.get_id() == "compute" && prefix == "--compute=")
        );
        let (_, found) = target(&tree, &words("-o"), "");
        let Target::Value { arg, .. } = found else {
            panic!("{found:?}")
        };
        assert_eq!(
            values_of(&choices(arg)),
            vec!["table", "json", "yaml", "csv"]
        );
    }

    #[test]
    fn completes_flags_of_the_command() {
        let tree = command_tree();
        let (_, found) = target(&tree, &words("diff sales.orders"), "--");
        let Target::Flag(command) = found else {
            panic!("{found:?}")
        };
        let names = flags(command);
        assert!(values_of(&names).contains(&"--from"));
        assert!(values_of(&names).contains(&"--profile"));
    }
}
