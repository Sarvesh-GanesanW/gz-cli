//! The read-only sandbox profile: what a command or script gz runs without
//! asking (see [`crate::assistant::read_only`]) may touch.
//!
//! On top of the usual limits it may write nowhere but gz's own temp
//! folder, has no network (a script keeps only its loopback bridge to gz),
//! and may connect only to the system's name-lookup sockets. On macOS it
//! also can't signal processes it didn't start, write any app's settings,
//! use the clipboard or mount and eject disks, can't read the contents of
//! secret files (`.env`, keys) anywhere, and a script can't read the
//! contents of files under home outside the open folders and its
//! interpreter's install folder, so code nobody looked at can't print what
//! the file tools would refuse to read. On Linux, bubblewrap gives it its
//! own process and network namespaces instead.

use std::path::{Path, PathBuf};

use super::{real, seatbelt_string, sockets, subpaths, Rules, CREDENTIAL_PATHS};
use crate::assistant::sandbox::{SECRET_EXTENSIONS, SECRET_FILE_NAMES, SECRET_FILE_PREFIXES};

/// What runs under the read-only profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadOnlyKind {
    /// A command from the read-only list.
    Command,
    /// A script nobody approved: its reads under home are limited too.
    Script,
}

/// What this machine's read-only profile can do; see
/// [`super::Confinement::read_only_support`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadOnlySupport {
    /// Secret files' contents are unreadable, so commands that read whole
    /// folders (`grep -r`, `rg`) can run without asking.
    pub hides_secrets: bool,
    /// Scripts can run read-only and still reach gz's bridge on loopback
    /// (bubblewrap's network namespace would cut it).
    pub scripts: bool,
}

/// What a read-only script may reach: reads under home, and its bridge.
pub struct ScriptReads<'a> {
    pub home: Option<&'a Path>,
    pub roots: &'a [PathBuf],
    /// The interpreter, by its path on `PATH`.
    pub program: &'a Path,
    /// The script's bridge to gz, the one loopback port it may reach.
    pub bridge_port: Option<u16>,
}

/// Narrows `rules` to the read-only profile: writes only in gz's own temp
/// folder (the command's `TMPDIR`), no network, no unix sockets beyond
/// name lookup, no keychain.
pub fn restrict(rules: &mut Rules, socket_folder: &Path) {
    rules.writable = vec![socket_folder.to_path_buf()];
    rules.protected.clear();
    rules.network = false;
    rules.keychain = false;
    let mut open: Vec<PathBuf> = sockets::SYSTEM_SOCKETS.iter().map(PathBuf::from).collect();
    open.push(socket_folder.to_path_buf());
    rules.sockets = open;
}

/// The `sandbox-exec` profile for read-only `rules`.
pub fn seatbelt_profile(rules: &Rules, kind: ReadOnlyKind, reads: &ScriptReads) -> String {
    let mut profile = super::seatbelt_profile(rules);
    profile.push_str(&outside_effects());
    profile.push_str(&loopback(kind, reads.bridge_port));
    if kind == ReadOnlyKind::Script {
        if let Some(home) = reads.home {
            profile.push_str(&home_reads(rules, home, reads));
        }
    }
    profile.push_str(&format!(
        "(deny file-read-data {})\n",
        secret_patterns().join(" ")
    ));
    profile
}

/// Closes what changes things outside the file system: signals to
/// processes the run didn't start, preference writes (which `cfprefsd`
/// makes for the caller), the clipboard, and mounting or ejecting disks.
fn outside_effects() -> String {
    let services: String = CLOSED_SERVICES
        .iter()
        .map(|name| format!(" (global-name \"{name}\")"))
        .collect();
    format!(
        "(deny signal)\n(allow signal (target same-sandbox))\n\
         (deny user-preference-write)\n(deny mach-lookup{services})\n"
    )
}

/// System services a read-only run may not reach: the clipboard, and disk
/// mounting and ejecting.
const CLOSED_SERVICES: &[&str] = &[
    "com.apple.pasteboard.1",
    "com.apple.DiskArbitration.diskarbitrationd",
];

/// Closes loopback, which the usual profile leaves open for local
/// servers: a read-only run could change a local database or dev server
/// through it. A script keeps its bridge to gz.
fn loopback(kind: ReadOnlyKind, bridge_port: Option<u16>) -> String {
    let mut text = String::from("(deny network-outbound (remote ip \"localhost:*\"))\n");
    if let (ReadOnlyKind::Script, Some(port)) = (kind, bridge_port) {
        text.push_str(&format!(
            "(allow network-outbound (remote ip \"localhost:{port}\"))\n"
        ));
    }
    text
}

/// Closes file contents under home but the open folders and the
/// interpreter's install folder; names and sizes stay readable, so path
/// lookups work. The credential stores close again after, since an open
/// folder may be home itself.
fn home_reads(rules: &Rules, home: &Path, reads: &ScriptReads) -> String {
    let mut open: Vec<PathBuf> = reads.roots.to_vec();
    open.extend(install_folder(reads.program));
    let mut text = format!(
        "(deny file-read-data (subpath {}))\n",
        seatbelt_string(home)
    );
    text.push_str(&format!("(allow file-read-data{})\n", subpaths(&open)));
    if !rules.hidden.is_empty() {
        text.push_str(&format!(
            "(deny file-read* file-write*{})\n",
            subpaths(&rules.hidden)
        ));
    }
    if !rules.readable.is_empty() {
        text.push_str(&format!(
            "(allow file-read*{})\n",
            subpaths(&rules.readable)
        ));
    }
    text
}

/// The folder an interpreter is installed in: the parent of the `bin`
/// folder its real path is in (`~/.pyenv/versions/3.12` for
/// `~/.pyenv/versions/3.12/bin/python3.12`).
fn install_folder(program: &Path) -> Option<PathBuf> {
    let real_program = real(program);
    let bin = real_program.parent()?;
    let prefix = if bin.file_name().is_some_and(|name| name == "bin") {
        bin.parent()?
    } else {
        bin
    };
    // Never the whole home or disk: an interpreter loose in either keeps
    // its scripts closed.
    (prefix.parent().is_some() && prefix.components().count() > 3).then(|| prefix.to_path_buf())
}

/// Seatbelt regexes for the files [`crate::assistant::sandbox`] calls
/// secret, matched on the real path, any case: names, prefixes, key
/// extensions, and the credential stores wherever they are.
fn secret_patterns() -> Vec<String> {
    let mut patterns = Vec::new();
    for prefix in SECRET_FILE_PREFIXES {
        patterns.push(format!("/{}[^/]*$", any_case(prefix)));
    }
    for name in SECRET_FILE_NAMES {
        patterns.push(format!("/{}$", any_case(name)));
    }
    for extension in SECRET_EXTENSIONS {
        patterns.push(format!("[^/]\\.{}$", any_case(extension)));
    }
    for store in CREDENTIAL_PATHS {
        patterns.push(format!("/{}(/|$)", any_case(store)));
    }
    patterns
        .into_iter()
        .map(|pattern| format!("(regex #\"{pattern}\")"))
        .collect()
}

/// `text` as a regex matching it in any case (`.env` → `\.[eE][nN][vV]`).
fn any_case(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            c if c.is_ascii_alphabetic() => {
                format!("[{}{}]", c.to_ascii_lowercase(), c.to_ascii_uppercase())
            }
            c if c.is_ascii_alphanumeric() || c == '/' || c == '_' || c == '-' || c == ' ' => {
                c.to_string()
            }
            c => format!("\\{c}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules() -> Rules {
        Rules {
            writable: vec![PathBuf::from("/work")],
            hidden: vec![PathBuf::from("/home/u/.aws")],
            readable: vec![],
            protected: vec![PathBuf::from("/work/.git/hooks")],
            network: true,
            sockets: vec![PathBuf::from("/work")],
            keychain: true,
        }
    }

    #[test]
    fn restricting_leaves_only_gz_temp_writable_and_closes_the_network() {
        let mut rules = rules();
        restrict(&mut rules, Path::new("/tmp/gz-sandbox-1"));
        assert_eq!(rules.writable, vec![PathBuf::from("/tmp/gz-sandbox-1")]);
        assert!(!rules.network && !rules.keychain);
        assert!(!rules.sockets.contains(&PathBuf::from("/work")));
        assert!(rules.protected.is_empty());
    }

    #[test]
    fn any_case_escapes_and_folds_letters() {
        assert_eq!(any_case(".env"), "\\.[eE][nN][vV]");
        assert_eq!(any_case("id_rsa"), "[iI][dD]_[rR][sS][aA]");
    }

    #[test]
    fn only_scripts_lose_reads_under_home() {
        let mut rules = rules();
        restrict(&mut rules, Path::new("/tmp/gz"));
        let roots = [PathBuf::from("/home/u/app")];
        let reads = ScriptReads {
            home: Some(Path::new("/home/u")),
            roots: &roots,
            program: Path::new("/usr/bin/python3"),
            bridge_port: Some(4100),
        };
        let command = seatbelt_profile(&rules, ReadOnlyKind::Command, &reads);
        let script = seatbelt_profile(&rules, ReadOnlyKind::Script, &reads);
        assert!(!command.contains("(deny file-read-data (subpath \"/home/u\"))"));
        assert!(script.contains("(deny file-read-data (subpath \"/home/u\"))"));
        assert!(script.contains("(allow file-read-data (subpath \"/home/u/app\")"));
        // The stores close again after the open folders reopen.
        let reopen = script.find("(allow file-read-data").unwrap();
        let hide = script.rfind("(deny file-read* file-write* (subpath \"/home/u/.aws\"))");
        assert!(hide.is_some_and(|at| at > reopen), "{script}");
        assert!(command.contains("(deny file-read-data (regex"));
    }

    #[test]
    fn both_kinds_close_signals_settings_and_loopback_but_a_script_keeps_its_bridge() {
        let mut rules = rules();
        restrict(&mut rules, Path::new("/tmp/gz"));
        let reads = ScriptReads {
            home: None,
            roots: &[],
            program: Path::new("/usr/bin/python3"),
            bridge_port: Some(4100),
        };
        let command = seatbelt_profile(&rules, ReadOnlyKind::Command, &reads);
        let script = seatbelt_profile(&rules, ReadOnlyKind::Script, &reads);
        for profile in [&command, &script] {
            assert!(profile.contains("(deny signal)\n(allow signal (target same-sandbox))"));
            assert!(profile.contains("(deny user-preference-write)"));
            assert!(profile.contains("(global-name \"com.apple.pasteboard.1\")"));
            let open = profile.find("(allow network-outbound (remote ip \"localhost:*\"))");
            let closed = profile.find("(deny network-outbound (remote ip \"localhost:*\"))");
            assert!(
                closed > open,
                "loopback closes after the base opens it: {profile}"
            );
        }
        let bridge = "(allow network-outbound (remote ip \"localhost:4100\"))";
        assert!(script.contains(bridge) && !command.contains(bridge));
    }

    #[test]
    fn an_interpreter_is_installed_above_its_bin_folder() {
        let dir = crate::assistant::sandbox::tests::temp_dir("ro-install");
        let bin = dir.join("versions/3.12/bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("python3"), "").unwrap();
        assert_eq!(
            install_folder(&bin.join("python3")),
            Some(dir.join("versions/3.12"))
        );
        assert_eq!(install_folder(Path::new("/python3")), None);
    }
}

/// Real commands and scripts through the real macOS read-only profile.
#[cfg(all(test, target_os = "macos"))]
mod macos_tests {
    use super::super::test_support::*;
    use super::super::{Confinement, OsSandbox, SandboxSettings};
    use super::*;
    use crate::assistant::jobs::Jobs;
    use crate::assistant::sandbox::Sandbox;

    fn read_only_jobs(place: &Place) -> Jobs {
        let folders = Sandbox::with_home(&place.work, Some(&place.home)).unwrap();
        let confinement = Confinement::new(folders, OsSandbox::unsaved(SandboxSettings::default()));
        Jobs::new()
            .0
            .confined(confinement.read_only(ReadOnlyKind::Command))
    }

    #[tokio::test]
    async fn a_read_only_command_reads_the_folder_but_writes_nothing() {
        let place = place("ro-writes");
        std::fs::write(place.work.join("a.txt"), "hello").unwrap();
        let jobs = read_only_jobs(&place);
        let read = run(&jobs, &place, "cat a.txt").await;
        assert!(
            !read.is_error && read.output.ends_with("hello"),
            "{}",
            read.output
        );
        let write = run(&jobs, &place, "echo x > b.txt").await;
        assert!(write.is_error, "{}", write.output);
        assert!(!place.work.join("b.txt").exists());
        let temp = run(&jobs, &place, "echo x > /private/tmp/gz-ro-test.txt").await;
        assert!(temp.is_error, "system temp is closed too: {}", temp.output);
    }

    #[tokio::test]
    async fn a_read_only_command_cannot_read_secret_files() {
        let place = place("ro-secrets");
        std::fs::write(place.work.join(".env.local"), "TOKEN=1").unwrap();
        std::fs::write(place.work.join("server.PEM"), "key").unwrap();
        std::fs::write(place.work.join("a.txt"), "TOKEN=0").unwrap();
        let jobs = read_only_jobs(&place);
        let grep = run(&jobs, &place, "grep -r TOKEN .").await;
        assert!(grep.output.contains("a.txt"), "{}", grep.output);
        assert!(!grep.output.contains("TOKEN=1"), "{}", grep.output);
        let pem = run(&jobs, &place, "cat server.PEM").await;
        assert!(
            pem.is_error && !pem.output.contains("key\n"),
            "{}",
            pem.output
        );
        let listed = run(&jobs, &place, "ls -a").await;
        assert!(listed.output.contains(".env.local"), "names stay visible");
    }

    #[tokio::test]
    async fn a_read_only_command_cannot_signal_other_processes() {
        let place = place("ro-signal");
        let jobs = read_only_jobs(&place);
        let mut outside = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let out = run(&jobs, &place, &format!("kill -9 {}", outside.id())).await;
        let still_running = outside.try_wait().unwrap().is_none();
        outside.kill().ok();
        outside.wait().ok();
        assert!(out.is_error && still_running, "{}", out.output);
        let own = run(&jobs, &place, "sleep 5 & kill $!").await;
        assert!(
            !own.is_error,
            "its own children stay in reach: {}",
            own.output
        );
    }

    #[tokio::test]
    async fn a_read_only_command_cannot_reach_local_servers() {
        let place = place("ro-loopback");
        let jobs = read_only_jobs(&place);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let out = run(
            &jobs,
            &place,
            &format!(
                "python3 -c 'import socket; socket.create_connection((\"127.0.0.1\", {port}), 3)'"
            ),
        )
        .await;
        assert!(out.is_error, "{}", out.output);
    }

    #[tokio::test]
    async fn a_read_only_command_has_no_network() {
        let place = place("ro-network");
        let jobs = read_only_jobs(&place);
        let out = run(
            &jobs,
            &place,
            "python3 -c 'import socket; socket.create_connection((\"1.1.1.1\", 53), 3)'",
        )
        .await;
        assert!(out.is_error, "{}", out.output);
    }
}
