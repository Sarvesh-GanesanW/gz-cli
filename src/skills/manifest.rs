//! `SKILL.md` per the Agent Skills spec: YAML frontmatter with a `name`
//! and a `description`, then a markdown body. Other frontmatter fields are
//! kept for display only; none of them grants anything (`allowed-tools` in
//! particular never changes what gz may do without asking).

use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;

/// The file every skill folder has.
pub const SKILL_FILE: &str = "SKILL.md";
pub const MAX_NAME_CHARS: usize = 64;
pub const MAX_DESCRIPTION_CHARS: usize = 1024;

/// A parsed `SKILL.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub name: String,
    pub description: String,
    /// Every other frontmatter field, rendered as text, for `gz skills show`.
    pub extra: BTreeMap<String, String>,
    /// The markdown after the frontmatter.
    pub body: String,
}

impl Manifest {
    /// Parses `text`; the name must be valid, and must equal `folder` when
    /// one is given (the folder a skill lives in names it).
    pub fn parse(text: &str, folder: Option<&str>) -> Result<Self> {
        let (front, body) = split_frontmatter(text)?;
        let fields: serde_yaml::Mapping =
            serde_yaml::from_str(front).context("the SKILL.md frontmatter is not valid YAML")?;
        let mut name = None;
        let mut description = None;
        let mut extra = BTreeMap::new();
        for (key, value) in fields {
            let Some(key) = key.as_str().map(str::to_string) else {
                bail!("the SKILL.md frontmatter has a key that is not text");
            };
            match key.as_str() {
                "name" => name = value.as_str().map(str::to_string),
                "description" => description = value.as_str().map(str::to_string),
                _ => {
                    extra.insert(key, display_value(&value));
                }
            }
        }
        let name = name.context("SKILL.md has no `name` in its frontmatter")?;
        check_name(&name)?;
        if let Some(folder) = folder {
            if folder != name {
                bail!("the skill is named '{name}' but its folder is '{folder}'; they must match");
            }
        }
        let description = description
            .map(|d| d.trim().to_string())
            .filter(|d| !d.is_empty())
            .context("SKILL.md has no `description` in its frontmatter")?;
        if description.chars().count() > MAX_DESCRIPTION_CHARS {
            bail!("the description of '{name}' is longer than {MAX_DESCRIPTION_CHARS} characters");
        }
        Ok(Self {
            name,
            description,
            extra,
            body: body.to_string(),
        })
    }
}

/// The spec's name rule: 1–64 lowercase letters, digits and single hyphens,
/// not starting or ending with a hyphen.
pub fn check_name(name: &str) -> Result<()> {
    let valid_chars = name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if name.is_empty()
        || name.len() > MAX_NAME_CHARS
        || !valid_chars
        || name.starts_with('-')
        || name.ends_with('-')
        || name.contains("--")
    {
        bail!(
            "'{name}' is not a valid skill name: use 1–{MAX_NAME_CHARS} lowercase letters, digits and single hyphens"
        );
    }
    Ok(())
}

/// `(frontmatter, body)`: the YAML between the opening and closing `---`.
fn split_frontmatter(text: &str) -> Result<(&str, &str)> {
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(text);
    let Some(rest) = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
    else {
        bail!("SKILL.md must start with `---` frontmatter (name and description)");
    };
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "---" {
            let body = &rest[offset + line.len()..];
            return Ok((&rest[..offset], body.trim_start_matches(['\r', '\n'])));
        }
        offset += line.len();
    }
    bail!("the SKILL.md frontmatter is not closed with `---`")
}

fn display_value(value: &serde_yaml::Value) -> String {
    match value {
        serde_yaml::Value::String(s) => s.clone(),
        serde_yaml::Value::Bool(b) => b.to_string(),
        serde_yaml::Value::Number(n) => n.to_string(),
        serde_yaml::Value::Null => String::new(),
        other => serde_yaml::to_string(other)
            .unwrap_or_default()
            .trim()
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PDF: &str = "---\nname: pdf-tools\ndescription: Read and fill PDF forms.\nallowed-tools: Bash(*)\nlicense: MIT\n---\n\n# PDF tools\nUse scripts/fill.py.\n";

    #[test]
    fn parses_name_description_body_and_keeps_other_fields_for_display() {
        let manifest = Manifest::parse(PDF, Some("pdf-tools")).unwrap();
        assert_eq!(manifest.name, "pdf-tools");
        assert_eq!(manifest.description, "Read and fill PDF forms.");
        assert_eq!(manifest.body, "# PDF tools\nUse scripts/fill.py.\n");
        assert_eq!(manifest.extra["allowed-tools"], "Bash(*)");
        assert_eq!(manifest.extra["license"], "MIT");
    }

    #[test]
    fn the_folder_must_match_the_name() {
        let err = Manifest::parse(PDF, Some("pdf")).unwrap_err().to_string();
        assert!(err.contains("folder is 'pdf'"), "{err}");
    }

    #[test]
    fn names_follow_the_spec() {
        for good in ["a", "pdf-tools", "x2", &"a".repeat(64)] {
            assert!(check_name(good).is_ok(), "{good}");
        }
        for bad in [
            "",
            "PDF",
            "-a",
            "a-",
            "a--b",
            "a_b",
            "a b",
            "../x",
            &"a".repeat(65),
        ] {
            assert!(check_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn rejects_missing_or_oversized_fields_and_broken_frontmatter() {
        let cases = [
            ("# no frontmatter", "must start with"),
            ("---\nname: a\ndescription: b\n", "not closed"),
            ("---\ndescription: b\n---\n", "no `name`"),
            ("---\nname: a\n---\n", "no `description`"),
            (
                "---\nname: a\ndescription: \"  \"\n---\n",
                "no `description`",
            ),
            ("---\nname: [a\n---\n", "not valid YAML"),
        ];
        for (text, expected) in cases {
            let err = format!("{:#}", Manifest::parse(text, None).unwrap_err());
            assert!(err.contains(expected), "{text:?}: {err}");
        }
        let long = format!("---\nname: a\ndescription: {}\n---\n", "d".repeat(1025));
        assert!(Manifest::parse(&long, None).is_err());
    }
}
