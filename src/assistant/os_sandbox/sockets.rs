//! Unix sockets a confined command may connect to.
//!
//! A connection to a local service runs with that service's powers, not the
//! command's: the ssh-agent signs with the user's keys, Docker's socket is
//! root on the machine, tmux's types into the user's shells, a browser's
//! opens pages as the user. So commands reach only sockets in the open
//! folders, in gz's own socket folder (their `TMPDIR`, so sockets a command
//! makes for itself keep working), the system's name lookup, and sockets
//! the user allowed for the session.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// Sockets every command needs: the system's name lookup (without it no
/// host name resolves) and its log.
#[cfg(target_os = "macos")]
pub const SYSTEM_SOCKETS: &[&str] = &["/private/var/run/mDNSResponder", "/private/var/run/syslog"];
/// Name lookup through systemd-resolved and nscd: read-only answers.
#[cfg(not(target_os = "macos"))]
pub const SYSTEM_SOCKETS: &[&str] = &[
    "/run/systemd/resolve/io.systemd.Resolve",
    "/run/nscd/socket",
    "/var/run/nscd/socket",
];

/// Where sockets of other network namespaces usually live (a container's
/// daemon, Docker Desktop's VM): `/proc/net/unix` lists only our own.
#[cfg(target_os = "linux")]
const SOCKET_FOLDERS: &[&str] = &["/tmp", "/run", "/var/run"];
#[cfg(target_os = "linux")]
const SOCKET_SEARCH_DEPTH: usize = 3;

/// gz's socket folder, made private on first use. Confined commands get it
/// as `TMPDIR`. Kept short, since a socket path holds only about 100 bytes.
pub fn socket_folder() -> Result<PathBuf> {
    #[cfg(unix)]
    {
        let base = if cfg!(target_os = "macos") {
            "/private/tmp"
        } else {
            "/tmp"
        };
        // SAFETY: getuid has no preconditions and cannot fail.
        let uid = unsafe { libc::getuid() };
        let folder = PathBuf::from(format!("{base}/gz-sandbox-{uid}"));
        ensure_private_folder(&folder, uid)?;
        Ok(folder)
    }
    #[cfg(not(unix))]
    {
        Ok(std::env::temp_dir())
    }
}

/// Makes `folder` owner-only, or checks that an existing one is: in a
/// shared `/tmp` another user could have made it first to read or plant
/// sockets.
#[cfg(unix)]
fn ensure_private_folder(folder: &Path, uid: u32) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    match std::fs::DirBuilder::new().mode(0o700).create(folder) {
        Ok(()) => return Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(err) => {
            return Err(err).with_context(|| format!("cannot create {}", folder.display()));
        }
    }
    let meta = std::fs::symlink_metadata(folder)
        .with_context(|| format!("cannot read {}", folder.display()))?;
    let is_private = meta.is_dir() && meta.uid() == uid && meta.mode() & 0o077 == 0;
    if !is_private {
        bail!(
            "{} is not a private folder of yours, so gz won't give it to commands; \
             remove it and gz makes its own",
            folder.display()
        );
    }
    Ok(())
}

/// The ssh-agent sockets ssh would use: `$SSH_AUTH_SOCK` and each
/// `IdentityAgent` in `~/.ssh/config`, by real path, those that exist.
pub fn ssh_agent_sockets(home: Option<&Path>, auth_sock: Option<&OsStr>) -> Vec<PathBuf> {
    let mut agents: Vec<PathBuf> = auth_sock
        .filter(|sock| !sock.is_empty())
        .map(PathBuf::from)
        .into_iter()
        .collect();
    if let Some(home) = home {
        if let Ok(config) = std::fs::read_to_string(home.join(".ssh/config")) {
            agents.extend(identity_agents(&config, home));
        }
    }
    let mut real: Vec<PathBuf> = agents
        .iter()
        .filter_map(|agent| agent.canonicalize().ok())
        .collect();
    super::dedup(&mut real);
    real
}

/// The `IdentityAgent` paths in an ssh config, `~` expanded. `none` and
/// `SSH_AUTH_SOCK` (meaning the variable) name no path of their own.
fn identity_agents(config: &str, home: &Path) -> Vec<PathBuf> {
    config
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let (key, value) = line
                .split_once(|c: char| c.is_whitespace() || c == '=')
                .map(|(key, value)| (key, value.trim_start_matches(['=', ' ', '\t'])))?;
            key.eq_ignore_ascii_case("identityagent")
                .then(|| value.trim().trim_matches('"'))
        })
        .filter(|value| !value.eq_ignore_ascii_case("none") && *value != "SSH_AUTH_SOCK")
        .map(|value| match value.strip_prefix("~/") {
            Some(rest) => home.join(rest),
            None => PathBuf::from(value),
        })
        .filter(|path| path.is_absolute())
        .collect()
}

/// Whether `path` is a unix socket (following symlinks).
pub fn is_socket(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        std::fs::metadata(path).is_ok_and(|meta| meta.file_type().is_socket())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
}

/// Absolute paths in a command's output, as in "dial unix
/// /var/run/docker.sock:", "unix:///…" or "error connecting to /tmp/x".
pub fn paths_in(output: &str) -> Vec<String> {
    let mut paths: Vec<String> = output
        .split(|c: char| c.is_whitespace() || "'\"`()<>,;[]{}".contains(c))
        .map(|token| token.strip_prefix("unix://").unwrap_or(token))
        .map(|token| token.trim_end_matches([':', '.']))
        .filter(|token| token.starts_with('/') && token.len() > 1)
        .map(str::to_string)
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

/// Unix sockets bound now to a path: those in `/proc/net/unix` (this
/// network namespace) and those in the usual socket folders.
#[cfg(target_os = "linux")]
pub fn bound_sockets() -> Vec<PathBuf> {
    let mut found = std::fs::read_to_string("/proc/net/unix")
        .map(|table| proc_net_unix_paths(&table))
        .unwrap_or_default();
    let mut folders: Vec<PathBuf> = SOCKET_FOLDERS.iter().map(PathBuf::from).collect();
    folders.extend(std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from));
    for folder in folders {
        sockets_below(&folder, SOCKET_SEARCH_DEPTH, &mut found);
    }
    found.sort();
    found.dedup();
    found
}

/// Linux only: elsewhere the sandbox closes sockets by rule, not by
/// covering each one.
#[cfg(not(target_os = "linux"))]
pub fn bound_sockets() -> Vec<PathBuf> {
    Vec::new()
}

#[cfg(target_os = "linux")]
fn sockets_below(folder: &Path, depth: usize, found: &mut Vec<PathBuf>) {
    use std::os::unix::fs::FileTypeExt;
    let Ok(entries) = std::fs::read_dir(folder) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_socket() {
            found.push(entry.path());
        } else if kind.is_dir() && depth > 1 {
            sockets_below(&entry.path(), depth - 1, found);
        }
    }
}

/// The path column of `/proc/net/unix`; abstract sockets (`@name`) and
/// unnamed ones have none.
#[cfg(any(target_os = "linux", test))]
fn proc_net_unix_paths(table: &str) -> Vec<PathBuf> {
    // Num RefCount Protocol Flags Type St Inode Path
    const PATH_COLUMN: usize = 7;
    table
        .lines()
        .skip(1)
        .filter_map(|line| line.split_whitespace().nth(PATH_COLUMN))
        .filter(|path| path.starts_with('/'))
        .map(PathBuf::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_agents_are_read_from_the_ssh_config() {
        let config = "Host *\n  IdentityAgent \"~/Library/Group Containers/op/agent.sock\"\n\
                      Host work\n  identityagent=/run/user/1/gnupg/S.gpg-agent.ssh\n\
                      Host none\n  IdentityAgent none\nHost env\n  IdentityAgent SSH_AUTH_SOCK\n\
                      Host rel\n  IdentityAgent relative.sock\n";
        let home = Path::new("/home/me");
        assert_eq!(
            identity_agents(config, home),
            vec![
                home.join("Library/Group Containers/op/agent.sock"),
                PathBuf::from("/run/user/1/gnupg/S.gpg-agent.ssh"),
            ]
        );
    }

    #[test]
    fn paths_are_found_in_the_ways_tools_print_them() {
        let output = "Cannot connect to the Docker daemon at unix:///var/run/docker.sock. \
                      Is it running?\ndial unix /tmp/x.sock: connect: operation not permitted\n\
                      error connecting to /tmp/tmux-501/default (Operation not permitted)\n\
                      cat: '/home/me/.ssh/id_ed25519': Permission denied";
        assert_eq!(
            paths_in(output),
            [
                "/home/me/.ssh/id_ed25519",
                "/tmp/tmux-501/default",
                "/tmp/x.sock",
                "/var/run/docker.sock",
            ]
        );
    }

    #[test]
    fn proc_net_unix_gives_path_sockets_only() {
        let table = "Num       RefCount Protocol Flags    Type St Inode Path\n\
                     0000: 00000002 00000000 00010000 0001 01 2001 /run/user/1000/bus\n\
                     0000: 00000002 00000000 00010000 0001 01 2002 @/tmp/.X11-unix/X0\n\
                     0000: 00000003 00000000 00000000 0001 03 2003\n\
                     0000: 00000002 00000000 00010000 0001 01 2004 /tmp/ssh-x/agent.12\n";
        assert_eq!(
            proc_net_unix_paths(table),
            [
                PathBuf::from("/run/user/1000/bus"),
                PathBuf::from("/tmp/ssh-x/agent.12")
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_socket_folder_someone_else_could_reach_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let base = crate::assistant::sandbox::tests::temp_dir("socket-folder");
        let uid = unsafe { libc::getuid() };
        let fresh = base.join("fresh");
        ensure_private_folder(&fresh, uid).unwrap();
        let mode = std::fs::metadata(&fresh).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
        ensure_private_folder(&fresh, uid).unwrap();
        std::fs::set_permissions(&fresh, std::fs::Permissions::from_mode(0o777)).unwrap();
        let why = ensure_private_folder(&fresh, uid).unwrap_err().to_string();
        assert!(why.contains("not a private folder"), "{why}");
        let file = base.join("file");
        std::fs::write(&file, "").unwrap();
        assert!(ensure_private_folder(&file, uid).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn ssh_agents_come_from_the_variable_and_the_config_when_they_exist() {
        let home = crate::assistant::sandbox::tests::temp_dir("ssh-agents");
        let short = std::path::PathBuf::from(format!("/tmp/gz-agents-{}", std::process::id()));
        std::fs::create_dir_all(&short).unwrap();
        let env_sock = short.join("env.sock");
        let config_sock = short.join("cfg.sock");
        let _a = std::os::unix::net::UnixListener::bind(&env_sock).unwrap();
        let _b = std::os::unix::net::UnixListener::bind(&config_sock).unwrap();
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::write(
            home.join(".ssh/config"),
            format!(
                "IdentityAgent {}\nHost x\n  IdentityAgent /gone.sock\n",
                config_sock.display()
            ),
        )
        .unwrap();
        let found = ssh_agent_sockets(Some(&home), Some(env_sock.as_os_str()));
        assert_eq!(
            found,
            [
                env_sock.canonicalize().unwrap(),
                config_sock.canonicalize().unwrap()
            ]
        );
        assert!(is_socket(&env_sock) && !is_socket(&home));
        std::fs::remove_dir_all(&short).ok();
    }
}
