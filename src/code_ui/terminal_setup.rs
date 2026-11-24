//! `/terminal-setup`: making Shift+Enter add a newline in the terminal gz
//! runs in.
//!
//! gz asks terminals for unambiguous key reports, so terminals that support
//! that (kitty, Ghostty) already report Shift+Enter. VS Code-family editors'
//! terminals and Alacritty are configured to send Esc+Return for
//! Shift+Enter, which gz reads as Alt+Enter, a newline: the change is shown
//! in full and made only after the user agrees, with a backup first.
//! Terminals whose settings gz should not edit (iTerm2, WezTerm, Apple
//! Terminal, tmux) get the steps to follow.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// The terminals `/terminal-setup` knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Terminal {
    ITerm2,
    /// VS Code or an editor built on it; the name is its settings folder
    /// (`Code`, `Code - Insiders`, `Cursor`, `Windsurf`).
    VsCode(&'static str),
    Ghostty,
    WezTerm,
    Alacritty,
    Kitty,
    AppleTerminalApp,
    Tmux,
    Unknown(Option<String>),
}

impl Terminal {
    pub fn name(&self) -> String {
        match self {
            Self::ITerm2 => "iTerm2".into(),
            Self::VsCode("Code") => "VS Code".into(),
            Self::VsCode("Code - Insiders") => "VS Code Insiders".into(),
            Self::VsCode(folder) => (*folder).to_string(),
            Self::Ghostty => "Ghostty".into(),
            Self::WezTerm => "WezTerm".into(),
            Self::Alacritty => "Alacritty".into(),
            Self::Kitty => "kitty".into(),
            Self::AppleTerminalApp => "Terminal".into(),
            Self::Tmux => "tmux".into(),
            Self::Unknown(Some(program)) => program.clone(),
            Self::Unknown(None) => "this terminal".into(),
        }
    }

    /// The terminal gz runs in, from the variables terminals set.
    pub fn detect(var: &dyn Fn(&str) -> Option<String>) -> Self {
        let program = var("TERM_PROGRAM").filter(|p| !p.is_empty());
        if var("TMUX").is_some() || program.as_deref() == Some("tmux") {
            return Self::Tmux;
        }
        let term = var("TERM").unwrap_or_default();
        match program.as_deref() {
            Some("iTerm.app") => return Self::ITerm2,
            Some("vscode") => return Self::VsCode(vscode_flavor(var)),
            Some("ghostty") => return Self::Ghostty,
            Some("WezTerm") => return Self::WezTerm,
            Some("Apple_Terminal") => return Self::AppleTerminalApp,
            _ => {}
        }
        if var("KITTY_WINDOW_ID").is_some() || term == "xterm-kitty" {
            return Self::Kitty;
        }
        if var("ALACRITTY_WINDOW_ID").is_some()
            || var("ALACRITTY_SOCKET").is_some()
            || term == "alacritty"
        {
            return Self::Alacritty;
        }
        if var("GHOSTTY_RESOURCES_DIR").is_some() || term == "xterm-ghostty" {
            return Self::Ghostty;
        }
        if var("WEZTERM_EXECUTABLE").is_some() {
            return Self::WezTerm;
        }
        Self::Unknown(program)
    }
}

/// Which VS Code-family editor runs the terminal (they all set
/// `TERM_PROGRAM=vscode`).
fn vscode_flavor(var: &dyn Fn(&str) -> Option<String>) -> &'static str {
    let askpass = var("VSCODE_GIT_ASKPASS_NODE").unwrap_or_default();
    if var("CURSOR_TRACE_ID").is_some() || askpass.contains("Cursor") {
        "Cursor"
    } else if askpass.contains("Windsurf") {
        "Windsurf"
    } else if var("TERM_PROGRAM_VERSION").is_some_and(|v| v.contains("insider")) {
        "Code - Insiders"
    } else {
        "Code"
    }
}

/// Where settings folders are, from the environment.
#[derive(Debug, Clone)]
pub struct Places {
    pub home: PathBuf,
    /// `$XDG_CONFIG_HOME`, when set.
    pub xdg_config: Option<PathBuf>,
    /// `%APPDATA%` on Windows.
    pub app_data: Option<PathBuf>,
}

impl Places {
    pub fn from_env() -> Option<Self> {
        let home = directories::BaseDirs::new()?.home_dir().to_path_buf();
        let path = |name: &str| {
            std::env::var_os(name)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        };
        Some(Self {
            home,
            xdg_config: path("XDG_CONFIG_HOME"),
            app_data: path("APPDATA"),
        })
    }

    fn config_home(&self) -> PathBuf {
        self.xdg_config
            .clone()
            .unwrap_or_else(|| self.home.join(".config"))
    }

    /// The VS Code-family editor's `keybindings.json`.
    fn vscode_keybindings(&self, folder: &str) -> PathBuf {
        let user = if cfg!(target_os = "macos") {
            self.home.join("Library/Application Support").join(folder)
        } else if cfg!(windows) {
            self.app_data
                .clone()
                .unwrap_or_else(|| self.home.join("AppData/Roaming"))
                .join(folder)
        } else {
            self.config_home().join(folder)
        };
        user.join("User").join("keybindings.json")
    }

    /// Alacritty's config files, in the order it reads them.
    fn alacritty_configs(&self) -> Vec<PathBuf> {
        let mut places = vec![self.config_home().join("alacritty/alacritty.toml")];
        if let Some(app_data) = &self.app_data {
            places.push(app_data.join("alacritty/alacritty.toml"));
        }
        places.push(self.home.join(".alacritty.toml"));
        places
    }
}

/// A change to one settings file, shown before it is made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEdit {
    pub terminal: Terminal,
    pub path: PathBuf,
    /// The file as read (`None`: it doesn't exist yet).
    pub before: Option<String>,
    pub after: String,
}

impl FileEdit {
    /// The change as unified-diff lines.
    pub fn diff(&self) -> Vec<String> {
        let before = self.before.as_deref().unwrap_or_default();
        similar::TextDiff::from_lines(before, &self.after)
            .unified_diff()
            .context_radius(2)
            .to_string()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// Where the backup goes (next to the file, stamped with `now`).
    pub fn backup_path(&self, stamp: &str) -> PathBuf {
        let name = self
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.path
            .with_file_name(format!("{name}.gz-backup-{stamp}"))
    }

    /// Backs the file up and writes the change, refusing when the file
    /// changed since it was read. Returns the backup's path, if any.
    pub fn apply(&self, stamp: &str) -> Result<Option<PathBuf>> {
        let now = read_optional(&self.path)?;
        if now != self.before {
            bail!(
                "{} changed since gz read it; run /terminal-setup again to see the change against it",
                self.path.display()
            );
        }
        let backup = match &self.before {
            Some(_) => {
                let backup = self.backup_path(stamp);
                std::fs::copy(&self.path, &backup).with_context(|| {
                    format!(
                        "cannot back up {} to {}",
                        self.path.display(),
                        backup.display()
                    )
                })?;
                Some(backup)
            }
            None => None,
        };
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("cannot create {}", dir.display()))?;
        }
        std::fs::write(&self.path, &self.after)
            .with_context(|| format!("cannot write {}", self.path.display()))?;
        Ok(backup)
    }
}

/// What `/terminal-setup` will do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Shift+Enter already reaches gz; nothing to change.
    Works { terminal: Terminal, why: String },
    /// The setting is there already.
    Configured { terminal: Terminal, path: PathBuf },
    /// A settings file to change, after the user agrees.
    Edit(FileEdit),
    /// Steps the user follows in the terminal's own settings.
    Manual {
        terminal: Terminal,
        steps: Vec<String>,
    },
}

/// Newlines that work in every terminal, offered with every plan.
pub const ALWAYS_WORKS: &str = "In any terminal, \\ then Enter or Ctrl+J also adds a newline.";

/// Works out what to do for `terminal`, reading its settings file if any.
pub fn plan(terminal: Terminal, places: &Places) -> Result<Plan> {
    let why = |s: &str| s.to_string();
    Ok(match terminal {
        Terminal::Kitty | Terminal::Ghostty => Plan::Works {
            why: why("it reports Shift+Enter to gz as its own key"),
            terminal,
        },
        Terminal::VsCode(folder) => vscode_plan(terminal.clone(), places.vscode_keybindings(folder))?,
        Terminal::Alacritty => alacritty_plan(places)?,
        Terminal::ITerm2 => Plan::Manual {
            terminal,
            steps: vec![
                "Open iTerm2 → Settings → Profiles → Keys → Key Mappings.".into(),
                "Press +, click Keyboard Shortcut and press Shift+Return.".into(),
                "Set Action to \"Send Hex Code\" and enter: 0x1b 0x0d".into(),
                "Press OK. Shift+Enter then adds a newline in gz.".into(),
            ],
        },
        Terminal::WezTerm => Plan::Manual {
            terminal,
            steps: vec![
                "Open your WezTerm config (~/.wezterm.lua or ~/.config/wezterm/wezterm.lua).".into(),
                "Before `return config`, add:  config.enable_kitty_keyboard = true".into(),
                "Save; WezTerm reloads it. Shift+Enter then adds a newline in gz.".into(),
            ],
        },
        Terminal::AppleTerminalApp => Plan::Manual {
            terminal,
            steps: vec![
                "Terminal can't send Shift+Enter differently from Enter.".into(),
                "Use Option+Enter instead: Terminal → Settings → Profiles → Keyboard → tick \"Use Option as Meta key\".".into(),
            ],
        },
        Terminal::Tmux => Plan::Manual {
            terminal,
            steps: vec![
                "tmux 3.5 or newer: add to ~/.tmux.conf:".into(),
                "  set -s extended-keys always".into(),
                "  set -s extended-keys-format csi-u".into(),
                "  set -as terminal-features 'xterm*:extkeys'".into(),
                "Then run  tmux source-file ~/.tmux.conf  (or restart tmux), and run /terminal-setup outside tmux for the terminal itself.".into(),
                "Older tmux can't pass Shift+Enter on to gz: type \\ then Enter, or Ctrl+J, for a newline (check with  tmux -V).".into(),
            ],
        },
        Terminal::Unknown(_) => Plan::Manual {
            terminal,
            steps: vec![
                "gz doesn't know this terminal's settings. If it can map a key to text, map Shift+Enter to Esc followed by Return (hex 1b 0d).".into(),
            ],
        },
    })
}

fn read_optional(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("cannot read {}", path.display())),
    }
}

/// The key binding VS Code-family terminals need: Esc+Return on Shift+Enter.
const VSCODE_BINDING: &str = r#"{
    "key": "shift+enter",
    "command": "workbench.action.terminal.sendSequence",
    "args": { "text": "\u001b\r" },
    "when": "terminalFocus"
  }"#;

fn vscode_plan(terminal: Terminal, path: PathBuf) -> Result<Plan> {
    let before = read_optional(&path)?;
    let text = before.clone().unwrap_or_default();
    if text.trim().is_empty() {
        return Ok(Plan::Edit(FileEdit {
            terminal,
            path,
            before,
            after: format!("[\n  {VSCODE_BINDING}\n]\n"),
        }));
    }
    let code = blank_comments(&text);
    let parsed: Option<Vec<serde_json::Value>> =
        serde_json::from_str(&without_trailing_commas(&code)).ok();
    let (Some(bindings), Some(after)) = (parsed, insert_last(&text, &code)) else {
        return Ok(Plan::Manual {
            steps: vec![
                format!(
                    "gz couldn't read {} as a list of key bindings, so it leaves it alone.",
                    path.display()
                ),
                format!("Add this entry to the list in that file:\n  {VSCODE_BINDING}"),
            ],
            terminal,
        });
    };
    if bindings.iter().any(is_newline_binding) {
        return Ok(Plan::Configured { terminal, path });
    }
    Ok(Plan::Edit(FileEdit {
        terminal,
        path,
        before,
        after,
    }))
}

fn is_newline_binding(binding: &serde_json::Value) -> bool {
    let field = |name: &str| {
        binding
            .get(name)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
    };
    field("key").eq_ignore_ascii_case("shift+enter")
        && field("command") == "workbench.action.terminal.sendSequence"
}

/// `text` with `//` and `/* */` comments outside strings turned into
/// spaces (newlines kept), so byte positions still match `text`.
fn blank_comments(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = bytes.to_vec();
    let mut i = 0;
    let mut in_string = false;
    while i < bytes.len() {
        let c = bytes[i];
        if in_string {
            match c {
                b'\\' => i += 1,
                b'"' => in_string = false,
                _ => {}
            }
            i += 1;
            continue;
        }
        let next = bytes.get(i + 1).copied();
        let end = match (c, next) {
            (b'"', _) => {
                in_string = true;
                i += 1;
                continue;
            }
            (b'/', Some(b'/')) => text[i..].find('\n').map_or(bytes.len(), |n| i + n),
            (b'/', Some(b'*')) => text[i + 2..].find("*/").map_or(bytes.len(), |n| i + n + 4),
            _ => {
                i += 1;
                continue;
            }
        };
        for byte in &mut out[i..end] {
            if *byte != b'\n' {
                *byte = b' ';
            }
        }
        i = end;
    }
    // Only ASCII bytes were replaced, by ASCII spaces, so this is UTF-8.
    String::from_utf8(out).unwrap_or_default()
}

fn without_trailing_commas(code: &str) -> String {
    let re = regex::Regex::new(r",(\s*[\]}])").expect("a fixed pattern");
    re.replace_all(code, "$1").into_owned()
}

/// `text` with the binding added as the list's last entry; `code` is
/// `text` with comments blanked. `None` when there is no closing `]`.
fn insert_last(text: &str, code: &str) -> Option<String> {
    let close = code.rfind(']')?;
    let last = code[..close].trim_end();
    let at = last.len();
    let lead = match last.chars().last()? {
        '[' | ',' => "\n  ",
        _ => ",\n  ",
    };
    Some(format!(
        "{}{lead}{VSCODE_BINDING}{}",
        &text[..at],
        &text[at..]
    ))
}

/// The Alacritty binding: Shift+Return sends Esc+Return.
const ALACRITTY_BINDING: &str =
    "[[keyboard.bindings]]\nkey = \"Return\"\nmods = \"Shift\"\nchars = \"\\u001B\\r\"\n";

fn alacritty_plan(places: &Places) -> Result<Plan> {
    let terminal = Terminal::Alacritty;
    let configs = places.alacritty_configs();
    let path = configs
        .iter()
        .find(|p| p.is_file())
        .cloned()
        .unwrap_or_else(|| configs[0].clone());
    let before = read_optional(&path)?;
    let manual = |why: String| Plan::Manual {
        terminal: Terminal::Alacritty,
        steps: vec![why, format!("Add this to it:\n{ALACRITTY_BINDING}")],
    };
    let yaml = path.with_extension("yml");
    if before.is_none() && yaml.is_file() {
        return Ok(manual(format!(
            "{} is the old YAML format; run `alacritty migrate` first, or add the binding there by hand.",
            yaml.display()
        )));
    }
    let text = before.clone().unwrap_or_default();
    let Ok(parsed) = text.parse::<toml::Table>() else {
        return Ok(manual(format!(
            "gz couldn't read {} as TOML, so it leaves it alone.",
            path.display()
        )));
    };
    if has_alacritty_binding(&parsed) {
        return Ok(Plan::Configured { terminal, path });
    }
    let gap = match text.as_str() {
        "" => "",
        t if t.ends_with("\n\n") => "",
        t if t.ends_with('\n') => "\n",
        _ => "\n\n",
    };
    let after = format!("{text}{gap}{ALACRITTY_BINDING}");
    if after.parse::<toml::Table>().is_err() {
        return Ok(manual(format!(
            "{} lists its key bindings in a way gz can't add to safely.",
            path.display()
        )));
    }
    Ok(Plan::Edit(FileEdit {
        terminal,
        path,
        before,
        after,
    }))
}

fn has_alacritty_binding(config: &toml::Table) -> bool {
    let bindings = config
        .get("keyboard")
        .and_then(|k| k.get("bindings"))
        .and_then(|b| b.as_array());
    bindings.into_iter().flatten().any(|binding| {
        let field = |name: &str| {
            binding
                .get(name)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
        };
        matches!(field("key"), "Return" | "Enter")
            && field("mods") == "Shift"
            && binding.get("chars").is_some()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        }
    }

    fn places(name: &str) -> Places {
        let home = crate::assistant::sandbox::tests::temp_dir(&format!("term-setup-{name}"));
        Places {
            xdg_config: Some(home.join(".config")),
            app_data: None,
            home,
        }
    }

    #[test]
    fn detects_each_terminal() {
        let cases: &[(&[(&str, &str)], Terminal)] = &[
            (&[("TERM_PROGRAM", "iTerm.app")], Terminal::ITerm2),
            (&[("TERM_PROGRAM", "vscode")], Terminal::VsCode("Code")),
            (
                &[("TERM_PROGRAM", "vscode"), ("CURSOR_TRACE_ID", "x")],
                Terminal::VsCode("Cursor"),
            ),
            (
                &[
                    ("TERM_PROGRAM", "vscode"),
                    ("TERM_PROGRAM_VERSION", "1.96.0-insider"),
                ],
                Terminal::VsCode("Code - Insiders"),
            ),
            (&[("TERM_PROGRAM", "ghostty")], Terminal::Ghostty),
            (&[("TERM_PROGRAM", "WezTerm")], Terminal::WezTerm),
            (&[("TERM", "alacritty")], Terminal::Alacritty),
            (&[("ALACRITTY_WINDOW_ID", "1")], Terminal::Alacritty),
            (&[("TERM", "xterm-kitty")], Terminal::Kitty),
            (
                &[("TERM_PROGRAM", "Apple_Terminal")],
                Terminal::AppleTerminalApp,
            ),
            (
                &[("TERM_PROGRAM", "iTerm.app"), ("TMUX", "/tmp/x")],
                Terminal::Tmux,
            ),
            (&[], Terminal::Unknown(None)),
        ];
        for (vars, expected) in cases {
            assert_eq!(&Terminal::detect(&env(vars)), expected, "{vars:?}");
        }
    }

    #[test]
    fn protocol_terminals_need_nothing_and_others_get_steps() {
        let places = places("steps");
        assert!(matches!(
            plan(Terminal::Kitty, &places).unwrap(),
            Plan::Works { .. }
        ));
        assert!(matches!(
            plan(Terminal::Ghostty, &places).unwrap(),
            Plan::Works { .. }
        ));
        for terminal in [
            Terminal::ITerm2,
            Terminal::WezTerm,
            Terminal::AppleTerminalApp,
            Terminal::Tmux,
        ] {
            let Plan::Manual { steps, .. } = plan(terminal.clone(), &places).unwrap() else {
                panic!("{terminal:?} gets steps");
            };
            assert!(!steps.is_empty());
        }
    }

    #[test]
    fn tmux_steps_ask_for_csi_u_keys() {
        let places = places("tmux");
        let Plan::Manual { steps, .. } = plan(Terminal::Tmux, &places).unwrap() else {
            panic!("tmux gets steps");
        };
        let text = steps.join("\n");
        assert!(text.contains("extended-keys always"), "{text}");
        assert!(text.contains("extended-keys-format csi-u"), "{text}");
        assert!(!text.contains("extended-keys on"), "{text}");
        assert!(text.contains("Ctrl+J"), "{text}");
    }

    fn vscode_edit(places: &Places, existing: Option<&str>) -> Plan {
        let path = places.vscode_keybindings("Code");
        if let Some(text) = existing {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, text).unwrap();
        }
        plan(Terminal::VsCode("Code"), places).unwrap()
    }

    fn parsed(text: &str) -> Vec<serde_json::Value> {
        serde_json::from_str(&without_trailing_commas(&blank_comments(text))).unwrap()
    }

    #[test]
    fn vscode_gets_a_new_keybindings_file() {
        let places = places("vscode-new");
        let Plan::Edit(edit) = vscode_edit(&places, None) else {
            panic!()
        };
        assert!(edit.before.is_none());
        let bindings = parsed(&edit.after);
        assert_eq!(bindings.len(), 1);
        assert!(is_newline_binding(&bindings[0]));
        assert_eq!(bindings[0]["args"]["text"], "\u{1b}\r");
        assert!(edit
            .diff()
            .iter()
            .any(|l| l.contains("+    \"key\": \"shift+enter\"")));
    }

    #[test]
    fn vscode_keeps_comments_and_entries_and_adds_one() {
        let places = places("vscode-jsonc");
        let existing = "// my keys\n[\n  { \"key\": \"ctrl+k\", \"command\": \"x\" }, // trailing\n  /* note ] */\n]\n";
        let Plan::Edit(edit) = vscode_edit(&places, Some(existing)) else {
            panic!()
        };
        assert!(edit
            .after
            .starts_with("// my keys\n[\n  { \"key\": \"ctrl+k\""));
        assert!(edit.after.contains("/* note ] */"));
        let bindings = parsed(&edit.after);
        assert_eq!(bindings.len(), 2);
        assert!(is_newline_binding(&bindings[1]));

        let backup = edit.apply("20261125-0100").unwrap().unwrap();
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), existing);
        assert_eq!(std::fs::read_to_string(&edit.path).unwrap(), edit.after);
        assert!(
            matches!(
                plan(Terminal::VsCode("Code"), &places).unwrap(),
                Plan::Configured { .. }
            ),
            "a second run changes nothing"
        );
    }

    #[test]
    fn a_file_changed_after_the_plan_is_left_alone() {
        let places = places("vscode-race");
        let Plan::Edit(edit) = vscode_edit(&places, Some("[]")) else {
            panic!()
        };
        std::fs::write(&edit.path, "[{\"key\":\"a\",\"command\":\"b\"}]").unwrap();
        let err = edit.apply("x").unwrap_err();
        assert!(
            format!("{err:#}").contains("run /terminal-setup again"),
            "{err:#}"
        );
        assert_eq!(
            std::fs::read_to_string(&edit.path).unwrap(),
            "[{\"key\":\"a\",\"command\":\"b\"}]"
        );
    }

    #[test]
    fn unreadable_keybindings_get_steps_not_an_edit() {
        let places = places("vscode-bad");
        let plan = vscode_edit(&places, Some("{ \"not\": \"a list\" }"));
        assert!(matches!(plan, Plan::Manual { .. }), "{plan:?}");
    }

    #[test]
    fn alacritty_gets_a_binding_appended_once() {
        let places = places("alacritty");
        let path = places.config_home().join("alacritty/alacritty.toml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "[font]\nsize = 13").unwrap();
        let Plan::Edit(edit) = plan(Terminal::Alacritty, &places).unwrap() else {
            panic!()
        };
        assert_eq!(edit.path, path);
        assert!(edit
            .after
            .starts_with("[font]\nsize = 13\n\n[[keyboard.bindings]]"));
        let table: toml::Table = edit.after.parse().unwrap();
        assert!(has_alacritty_binding(&table));
        assert_eq!(
            table["keyboard"]["bindings"][0]["chars"].as_str(),
            Some("\u{1b}\r")
        );
        edit.apply("s").unwrap();
        assert!(matches!(
            plan(Terminal::Alacritty, &places).unwrap(),
            Plan::Configured { .. }
        ));
    }

    #[test]
    fn alacritty_inline_bindings_get_steps() {
        let places = places("alacritty-inline");
        let path = places.config_home().join("alacritty/alacritty.toml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "[keyboard]\nbindings = []\n").unwrap();
        assert!(matches!(
            plan(Terminal::Alacritty, &places).unwrap(),
            Plan::Manual { .. }
        ));
    }
}
