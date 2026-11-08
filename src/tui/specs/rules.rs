//! Input rules several screens share, copied from the backends that
//! enforce them, so forms refuse bad input before the server does.

/// Projects, jobs, Designer objects, workspaces and notebooks: the Joi rule
/// `^[A-Za-z](?:[A-Za-z0-9]|-(?=[A-Za-z0-9]))*$` (common layer schemas),
/// written without the look-ahead Rust's regex lacks (the web's form).
pub const NAME_PATTERN: &str = "[A-Za-z][A-Za-z0-9]*(?:-[A-Za-z0-9]+)*";
pub const NAME_MESSAGE: &str = "Start with a letter; use letters, numbers and single hyphens, \
     not at the end";
/// The database columns are VARCHAR(100) and nothing else checks the length.
pub const NAME_MAX: usize = 100;

#[cfg(test)]
mod tests {
    use super::*;
    use regex::Regex;

    #[test]
    fn names_follow_the_backend_rule() {
        let rule = Regex::new(&format!("^(?:{NAME_PATTERN})$")).unwrap();
        for good in ["ETL", "Invoice-Extractor", "a1-b2-c3"] {
            assert!(rule.is_match(good), "{good}");
        }
        for bad in ["1abc", "a--b", "a-", "has space", "under_score", ""] {
            assert!(!rule.is_match(bad), "{bad}");
        }
    }
}
