//! What each argument on the command line names: an id that may be given
//! as a name (and is swapped for the id before clap reads it), a name gz
//! completes but passes on as typed, or another value completion knows.
//!
//! One table for every command, so declarations in modules owned by other
//! work stay untouched. `tests::every_role_names_a_real_argument` keeps it
//! in step with `cli.rs`.

use clap::{Arg, Command};

use super::Kind;

/// What a value on the command line stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// An id: a name is looked up and replaced by its id.
    Id(Kind),
    /// A list of values, aligned to the right of these kinds; `None` is a
    /// value left as typed (`[PROJECT] JOB NAME`).
    Ids(&'static [Option<Kind>]),
    /// `designer rename|delete <KIND> <ID>`: the kind comes from the line.
    DesignerId,
    /// A name the command already accepts; only completed.
    Name(Kind),
    /// Other values completion knows.
    Text(Text),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Text {
    Profile,
    Runtime,
    /// A database of the warehouse given in the `warehouse` argument.
    Namespace,
    /// A table of the `warehouse` and `namespace`/`database` arguments.
    TableIn,
    /// `WAREHOUSE.DATABASE.TABLE`, completed one part at a time.
    Table,
    /// `DATABASE.TABLE` in the `--warehouse` given or `gz sql`'s remembered one.
    DatabaseTable,
    /// A branch or tag of the table named on the line.
    Ref,
}

use Kind::*;

const PROJECT_JOB: &[Option<Kind>] = &[Some(Project), Some(Job)];
const PROJECT_JOB_TEXT: &[Option<Kind>] = &[Some(Project), Some(Job), None];

/// `(command, argument, role)`. A command ending in ` *` matches each of its
/// subcommands.
const ROLES: &[(&str, &str, Role)] = &[
    // projects and jobs
    ("projects get", "id", Role::Id(Project)),
    ("projects update", "id", Role::Id(Project)),
    ("projects rename", "id", Role::Id(Project)),
    ("projects set-owner", "id", Role::Id(Project)),
    ("projects delete", "id", Role::Id(Project)),
    ("projects jobs", "project", Role::Id(Project)),
    ("projects create-job", "project", Role::Id(Project)),
    ("projects job-exists", "project", Role::Id(Project)),
    ("projects job", "project", Role::Id(Project)),
    ("projects job", "job", Role::Id(Job)),
    ("projects update-job", "project", Role::Id(Project)),
    ("projects update-job", "job", Role::Id(Job)),
    ("projects rename-job", "args", Role::Ids(PROJECT_JOB_TEXT)),
    ("projects unpublish", "args", Role::Ids(PROJECT_JOB_TEXT)),
    ("projects delete-job", "ids", Role::Ids(PROJECT_JOB)),
    ("projects job-runs", "ids", Role::Ids(PROJECT_JOB)),
    ("jobs run", "project", Role::Id(Project)),
    ("jobs run", "job", Role::Id(Job)),
    ("jobs backfill", "project", Role::Id(Project)),
    ("jobs backfill", "job", Role::Id(Job)),
    ("jobs abort", "job_id", Role::Id(Job)),
    ("jobs current-run", "job_id", Role::Id(Job)),
    ("jobs insights", "job_id", Role::Id(Job)),
    ("jobs lineage-job", "job_id", Role::Id(Job)),
    ("jobs fix", "project", Role::Id(Project)),
    ("jobs fix", "job", Role::Name(Job)),
    ("jobs try", "project", Role::Id(Project)),
    ("jobs try", "job", Role::Name(Job)),
    ("jobs publish", "project", Role::Id(Project)),
    ("jobs publish", "job", Role::Name(Job)),
    ("jobs discard", "project", Role::Id(Project)),
    ("jobs discard", "job", Role::Name(Job)),
    ("fix", "project", Role::Id(Project)),
    ("review", "project", Role::Id(Project)),
    ("fix", "job", Role::Name(Job)),
    ("run", "project", Role::Id(Project)),
    ("run", "job", Role::Name(Job)),
    ("sync", "project", Role::Id(Project)),
    ("flows list", "project", Role::Id(Project)),
    ("flows create", "project", Role::Id(Project)),
    ("open", "project", Role::Id(Project)),
    ("impact", "project", Role::Id(Project)),
    ("quality check", "project", Role::Id(Project)),
    ("quality watch", "project", Role::Id(Project)),
    ("usage compute", "project", Role::Id(Project)),
    ("usage compute", "job", Role::Name(Job)),
    ("models delete", "project", Role::Id(Project)),
    ("agents import-publication", "project", Role::Id(Project)),
    // notebooks
    ("notebooks list", "project_id", Role::Id(Project)),
    ("notebooks create", "project_id", Role::Id(Project)),
    ("notebooks exists", "project_id", Role::Id(Project)),
    ("notebooks update", "project_id", Role::Id(Project)),
    ("notebooks update", "notebook_id", Role::Id(Notebook)),
    ("notebooks delete", "project_id", Role::Id(Project)),
    ("notebooks delete", "notebook_id", Role::Id(Notebook)),
    ("notebooks patch", "notebook_id", Role::Id(Notebook)),
    ("notebooks unpublish", "notebook_id", Role::Id(Notebook)),
    // designer
    ("datasets get", "id", Role::Id(Dataset)),
    ("designer dataset", "id", Role::Id(Dataset)),
    ("designer update-dataset", "id", Role::Id(Dataset)),
    ("designer dashboard", "id", Role::Id(Dashboard)),
    ("designer update-dashboard", "id", Role::Id(Dashboard)),
    ("designer unpublish", "dashboard", Role::Id(Dashboard)),
    ("designer visualization", "id", Role::Id(Visualization)),
    (
        "designer update-visualization",
        "id",
        Role::Id(Visualization),
    ),
    ("designer filter", "id", Role::Id(Filter)),
    ("designer update-filter", "id", Role::Id(Filter)),
    ("designer rename", "id", Role::DesignerId),
    ("designer delete", "id", Role::DesignerId),
    ("jobs lineage-dataset", "dataset_id", Role::Id(Dataset)),
    ("explain dashboard", "dashboard", Role::Name(Dashboard)),
    ("explain dataset", "dataset", Role::Name(Dataset)),
    ("explain viz", "visualization", Role::Name(Visualization)),
    // workspaces
    ("workspaces contents", "id", Role::Id(Workspace)),
    ("workspaces get", "id", Role::Id(Workspace)),
    ("workspaces rename", "id", Role::Id(Workspace)),
    ("workspaces move", "id", Role::Id(Workspace)),
    ("workspaces move", "parent", Role::Id(Workspace)),
    ("workspaces set-owner", "id", Role::Id(Workspace)),
    ("workspaces delete", "id", Role::Id(Workspace)),
    ("workspaces create-in", "folder", Role::Id(Workspace)),
    ("workspaces remove", "workspace", Role::Id(Workspace)),
    ("workspaces exists", "parent", Role::Id(Workspace)),
    ("workspaces dashboard", "parent", Role::Id(Workspace)),
    ("workspaces dashboard", "dashboard", Role::Id(Dashboard)),
    // agents
    ("agents *", "id", Role::Id(Agent)),
    ("agents add-sub-agent", "child", Role::Id(Agent)),
    // connections
    ("connections get", "id", Role::Id(Connection)),
    ("connections update", "id", Role::Id(Connection)),
    ("connections rename", "id", Role::Id(Connection)),
    ("connections delete", "id", Role::Id(Connection)),
    ("jobs lineage-assets", "connection", Role::Id(Connection)),
    ("jobs lineage export", "connection", Role::Id(Connection)),
    ("fs *", "connection", Role::Name(Connection)),
    ("quality check", "connection", Role::Name(Connection)),
    ("quality watch", "connection", Role::Name(Connection)),
    // compute
    ("jobs run", "compute", Role::Id(Compute)),
    ("jobs backfill", "compute", Role::Id(Compute)),
    ("jobs try", "compute", Role::Id(Compute)),
    ("run", "compute", Role::Id(Compute)),
    ("fix", "compute", Role::Id(Compute)),
    ("review", "compute", Role::Id(Compute)),
    ("jobs fix", "compute", Role::Id(Compute)),
    ("rte compute get", "id", Role::Id(Compute)),
    ("rte compute update", "id", Role::Id(Compute)),
    ("rte compute rename", "id", Role::Id(Compute)),
    ("rte compute delete", "id", Role::Id(Compute)),
    // warehouses and saved queries
    ("warehouse delete", "id", Role::Id(Warehouse)),
    ("warehouse describe", "id", Role::Id(Warehouse)),
    ("sql saved by-warehouse", "id", Role::Id(Warehouse)),
    ("sql saved get", "id", Role::Id(SavedQuery)),
    ("sql saved update", "id", Role::Id(SavedQuery)),
    ("sql saved delete", "id", Role::Id(SavedQuery)),
    ("sql saved rename", "id", Role::Id(SavedQuery)),
    // lineage assets
    ("jobs lineage asset", "id", Role::Id(LineageAsset)),
    ("jobs lineage movement", "id", Role::Id(LineageAsset)),
    ("jobs lineage impact", "id", Role::Id(LineageAsset)),
    ("jobs lineage versions", "id", Role::Id(LineageAsset)),
    // ontology models
    ("jobs ontology *", "id", Role::Id(OntologyModel)),
    // tables named database.table, with --warehouse
    ("diff", "table", Role::Text(Text::DatabaseTable)),
    ("bisect", "table", Role::Text(Text::DatabaseTable)),
    ("table versions", "table", Role::Text(Text::DatabaseTable)),
    // tables named WAREHOUSE.DATABASE.TABLE (or DATABASE.TABLE with --warehouse)
    ("table profile", "table", Role::Text(Text::Table)),
    ("table rollback", "table", Role::Text(Text::Table)),
    ("access why", "table", Role::Text(Text::Table)),
    ("govern scan", "table", Role::Text(Text::Table)),
];

/// What `arg` of the command at `key` (`jobs run`) names, if anything.
pub fn role_of(key: &str, command: &Command, arg: &Arg) -> Option<Role> {
    let id = arg.get_id().as_str();
    if let Some(role) = listed(key, id) {
        return Some(role);
    }
    let value_name = arg
        .get_value_names()
        .and_then(|names| names.first())
        .map(|name| name.as_str())
        .unwrap_or_default();
    let has = |name: &str| command.get_arguments().any(|a| a.get_id() == name);
    let text = match id {
        "profile" => Text::Profile,
        "rte" => Text::Runtime,
        "warehouse" => return Some(Role::Name(Warehouse)),
        "compute" => return Some(Role::Name(Compute)),
        "namespace" | "database" if has("warehouse") => Text::Namespace,
        "table" if has("warehouse") && (has("namespace") || has("database")) => Text::TableIn,
        _ if value_name == "REF" => Text::Ref,
        _ if value_name.starts_with("WAREHOUSE.DATABASE.TABLE") => Text::Table,
        _ => return None,
    };
    Some(Role::Text(text))
}

/// The project argument of a command taking `<PROJECT> <JOB>`, where a
/// job given alone stands for both (`gz jobs run DQC`).
pub fn project_before_job<'c>(key: &str, command: &'c Command) -> Option<&'c Arg> {
    let list = crate::resolve::walk::positionals(command);
    let [project, job, ..] = list.as_slice() else {
        return None;
    };
    let is = |arg: &Arg, kind: Kind| role_of(key, command, arg) == Some(Role::Id(kind));
    (is(project, Project) && is(job, Job)).then_some(*project)
}

fn listed(key: &str, arg: &str) -> Option<Role> {
    ROLES.iter().find_map(|(pattern, name, role)| {
        let matches = match pattern.strip_suffix(" *") {
            Some(parent) => key
                .strip_prefix(parent)
                .is_some_and(|rest| rest.starts_with(' ')),
            None => key == *pattern,
        };
        (matches && *name == arg).then_some(*role)
    })
}

/// Every `(command, argument)` pair the table names, for its test.
#[cfg(test)]
pub fn listed_pairs() -> impl Iterator<Item = (&'static str, &'static str)> {
    ROLES.iter().map(|(command, arg, _)| (*command, *arg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolve::walk::{command_tree, find_subcommand};

    fn command_at<'c>(root: &'c Command, key: &str) -> Option<&'c Command> {
        key.split(' ')
            .try_fold(root, |command, name| find_subcommand(command, name))
    }

    #[test]
    fn every_role_names_a_real_argument() {
        let tree = command_tree();
        for (key, arg) in listed_pairs() {
            let parent = key.strip_suffix(" *");
            let command = command_at(&tree, parent.unwrap_or(key))
                .unwrap_or_else(|| panic!("no command `gz {key}`"));
            let has = |command: &Command| command.get_arguments().any(|a| a.get_id() == arg);
            let found = match parent {
                Some(_) => command.get_subcommands().any(has),
                None => has(command),
            };
            assert!(found, "`gz {key}` has no argument `{arg}`");
        }
    }

    /// A numeric id argument with no role would quietly refuse names.
    #[test]
    fn every_numeric_id_argument_has_a_role() {
        // Numbers that are not ids of named things.
        const NOT_NAMED: &[&str] = &[
            "timeout",
            "limit_cents",
            "errors",
            "approval",
            "definition",
            "column_id",
            "version",
            "run",
            "offset",
            "snapshot",
        ];
        let tree = command_tree();
        let mut missing = Vec::new();
        visit(&tree, "", &mut |key, command| {
            for arg in command.get_arguments() {
                let id = arg.get_id().as_str();
                let numeric = matches!(
                    arg.get_value_parser().type_id(),
                    t if t == clap::builder::ValueParser::new(clap::value_parser!(i64)).type_id()
                        || t == clap::builder::ValueParser::new(clap::value_parser!(u64)).type_id()
                );
                if numeric
                    && !NOT_NAMED.contains(&id)
                    // A published model's id; its name is the next argument.
                    && key != "models unpublish"
                    && role_of(key, command, arg).is_none()
                {
                    missing.push(format!("gz {key} {id}"));
                }
            }
        });
        assert!(missing.is_empty(), "ids without a role: {missing:?}");
    }

    fn visit(command: &Command, key: &str, each: &mut impl FnMut(&str, &Command)) {
        each(key, command);
        for sub in command.get_subcommands() {
            let key = if key.is_empty() {
                sub.get_name().to_string()
            } else {
                format!("{key} {}", sub.get_name())
            };
            visit(sub, &key, each);
        }
    }

    #[test]
    fn wildcards_match_subcommands_only() {
        assert_eq!(listed("agents get", "id"), Some(Role::Id(Agent)));
        assert_eq!(listed("agents", "id"), None);
        assert_eq!(listed("agentsx get", "id"), None);
    }
}
