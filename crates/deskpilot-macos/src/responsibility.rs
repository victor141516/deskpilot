//! TCC attributes privacy permissions to the *responsible* process: for a
//! binary started from a terminal or by an MCP client that is the hosting app
//! (Terminal, iTerm, Claude…), so deskpilot would silently inherit — or lack —
//! the host's permissions and the system prompts would name the host.
//! Re-executing ourselves with the responsibility disclaimed makes deskpilot
//! its own TCC identity, so the permissions are granted to (and listed as)
//! deskpilot, whoever launches it.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;

unsafe extern "C" {
    fn responsibility_spawnattrs_setdisclaim(
        attrs: *mut libc::posix_spawnattr_t,
        disclaim: libc::c_int,
    ) -> libc::c_int;
    fn responsibility_get_pid_responsible_for_pid(pid: libc::pid_t) -> libc::pid_t;
}

/// Call first thing in `main`. Replaces the process image in place (same pid,
/// same stdin/stdout/stderr) unless this process is already responsible for
/// itself. On failure the process keeps running with the inherited
/// responsibility.
pub fn disclaim_responsibility() {
    let pid = unsafe { libc::getpid() };
    if unsafe { responsibility_get_pid_responsible_for_pid(pid) } != pid {
        reexec();
    }
}

/// Replaces this process with a fresh copy of itself (same arguments,
/// environment, pid and file descriptors) that is responsible for itself.
/// Returns only if that fails.
pub(crate) fn reexec() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let Ok(path) = CString::new(exe.as_os_str().as_bytes()) else {
        return;
    };
    let Ok(args) = std::env::args_os()
        .map(|a| CString::new(a.as_bytes()))
        .collect::<Result<Vec<_>, _>>()
    else {
        return;
    };
    let mut argv: Vec<*mut libc::c_char> = args.iter().map(|a| a.as_ptr() as *mut _).collect();
    argv.push(std::ptr::null_mut());
    unsafe {
        let mut attr: libc::posix_spawnattr_t = std::ptr::null_mut();
        if libc::posix_spawnattr_init(&mut attr) != 0 {
            return;
        }
        if responsibility_spawnattrs_setdisclaim(&mut attr, 1) == 0
            && libc::posix_spawnattr_setflags(&mut attr, libc::POSIX_SPAWN_SETEXEC as _) == 0
        {
            // Only returns on failure.
            libc::posix_spawn(
                std::ptr::null_mut(),
                path.as_ptr(),
                std::ptr::null(),
                &attr,
                argv.as_ptr(),
                *libc::_NSGetEnviron(),
            );
        }
        libc::posix_spawnattr_destroy(&mut attr);
    }
}
