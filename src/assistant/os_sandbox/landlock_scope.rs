//! Landlock scoping for abstract unix sockets on Linux.
//!
//! Abstract sockets (`@name`) have no file, so bubblewrap's mounts can't
//! hide them; only a separate network namespace would, and that takes the
//! network too. Landlock (Linux 6.12+, ABI 6) can scope them instead: a
//! process in the domain may connect only to abstract sockets made inside
//! it. The domain is entered just before bubblewrap starts and holds for
//! everything the command runs.

use std::io;

/// `LANDLOCK_CREATE_RULESET_VERSION`: asks for the ABI version.
const CREATE_RULESET_VERSION: libc::c_ulong = 1;
/// `LANDLOCK_SCOPE_ABSTRACT_UNIX_SOCKET`.
const SCOPE_ABSTRACT_UNIX_SOCKET: u64 = 1;
/// The first ABI with scopes.
const ABI_WITH_SCOPES: libc::c_long = 6;

/// `struct landlock_ruleset_attr`.
#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
    handled_access_net: u64,
    scoped: u64,
}

/// Whether this kernel can scope abstract sockets; the reason when not.
pub fn scoping_available() -> Result<(), String> {
    // SAFETY: with a null attribute and the version flag the call only
    // reports the ABI.
    let abi = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<RulesetAttr>(),
            0usize,
            CREATE_RULESET_VERSION,
        )
    };
    if abi < 0 {
        let err = io::Error::last_os_error();
        return Err(format!(
            "this kernel has no Landlock ({err}); it needs Linux 6.12 or later with Landlock on"
        ));
    }
    if abi < ABI_WITH_SCOPES {
        return Err(format!(
            "this kernel's Landlock (ABI {abi}) can't scope them; it needs Linux 6.12 or later"
        ));
    }
    Ok(())
}

/// Puts the calling process in a domain that may connect only to abstract
/// sockets made inside it. Async-signal-safe (raw syscalls only), so it can
/// run between fork and exec.
pub fn scope_abstract_sockets() -> io::Result<()> {
    let attr = RulesetAttr {
        handled_access_fs: 0,
        handled_access_net: 0,
        scoped: SCOPE_ABSTRACT_UNIX_SOCKET,
    };
    // SAFETY: plain syscalls on a valid attribute and our own descriptor;
    // no_new_privs only stops later execs gaining privileges.
    unsafe {
        if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
            return Err(io::Error::last_os_error());
        }
        let ruleset = libc::syscall(
            libc::SYS_landlock_create_ruleset,
            &attr as *const RulesetAttr,
            std::mem::size_of::<RulesetAttr>(),
            0u32,
        );
        if ruleset < 0 {
            return Err(io::Error::last_os_error());
        }
        let ruleset = ruleset as libc::c_int;
        let restricted = libc::syscall(libc::SYS_landlock_restrict_self, ruleset, 0u32);
        libc::close(ruleset);
        if restricted != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}
