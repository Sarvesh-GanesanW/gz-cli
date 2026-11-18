//! Names wherever gz takes an id: `gz jobs run DQC` for `gz jobs run 1999 3013`.
//!
//! Before clap reads the command line, [`rewrite`] finds each id argument
//! given as a name (see [`roles`]), looks the name up with one list call per
//! kind ([`lookup::Catalog`]), and puts the id in its place, so command
//! handlers keep taking ids. A value made only of digits is always an id
//! (a UUID, for agents).

pub mod lookup;
pub mod pick;
pub mod rewrite;
pub mod roles;
pub mod walk;

/// Something gz can name instead of numbering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Project,
    Job,
    Notebook,
    Dataset,
    Dashboard,
    Visualization,
    Filter,
    Agent,
    Connection,
    Compute,
    Warehouse,
    Workspace,
    SavedQuery,
    OntologyModel,
    /// Found by searching for the name; there is no list of every asset.
    LineageAsset,
}

impl Kind {
    pub const ALL: [Kind; 15] = [
        Kind::Project,
        Kind::Job,
        Kind::Notebook,
        Kind::Dataset,
        Kind::Dashboard,
        Kind::Visualization,
        Kind::Filter,
        Kind::Agent,
        Kind::Connection,
        Kind::Compute,
        Kind::Warehouse,
        Kind::Workspace,
        Kind::SavedQuery,
        Kind::OntologyModel,
        Kind::LineageAsset,
    ];

    /// A one-word name for cache keys: `saved-query`.
    pub fn key(self) -> String {
        self.noun().replace(' ', "-")
    }

    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.key() == key)
    }

    pub fn noun(self) -> &'static str {
        match self {
            Kind::Project => "project",
            Kind::Job => "job",
            Kind::Notebook => "notebook",
            Kind::Dataset => "dataset",
            Kind::Dashboard => "dashboard",
            Kind::Visualization => "visualization",
            Kind::Filter => "filter",
            Kind::Agent => "agent",
            Kind::Connection => "connection",
            Kind::Compute => "compute size",
            Kind::Warehouse => "warehouse",
            Kind::Workspace => "workspace",
            Kind::SavedQuery => "saved query",
            Kind::OntologyModel => "ontology model",
            Kind::LineageAsset => "lineage asset",
        }
    }

    /// The command that lists them, for "no such name" errors.
    pub fn list_command(self) -> &'static str {
        match self {
            Kind::Project => "gz projects list",
            Kind::Job => "gz projects jobs <project>",
            Kind::Notebook => "gz notebooks list <project>",
            Kind::Dataset => "gz datasets list",
            Kind::Dashboard => "gz designer dashboards",
            Kind::Visualization => "gz designer visualizations",
            Kind::Filter => "gz designer filters",
            Kind::Agent => "gz agents list",
            Kind::Connection => "gz connections list",
            Kind::Compute => "gz rte compute",
            Kind::Warehouse => "gz warehouse list",
            Kind::Workspace => "gz workspaces list",
            Kind::SavedQuery => "gz sql saved list",
            Kind::OntologyModel => "gz jobs ontology list",
            Kind::LineageAsset => "gz jobs lineage export",
        }
    }

    /// Jobs and notebooks live in a project.
    pub fn in_project(self) -> bool {
        matches!(self, Kind::Job | Kind::Notebook)
    }

    /// Whether `value` is already an id rather than a name.
    pub fn is_id(self, value: &str) -> bool {
        match self {
            Kind::Agent => looks_like_uuid(value),
            _ => !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()),
        }
    }
}

pub fn looks_like_uuid(text: &str) -> bool {
    text.len() == 36 && text.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digits_and_uuids_are_ids() {
        assert!(Kind::Project.is_id("1999"));
        assert!(!Kind::Project.is_id("DQC"));
        assert!(!Kind::Project.is_id("1999a"));
        assert!(!Kind::Project.is_id(""));
        assert!(Kind::Agent.is_id("1a37e97d-893f-47ea-af39-ef5ba85b6128"));
        assert!(!Kind::Agent.is_id("1999"));
    }
}
