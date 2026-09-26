//! Cross-platform process and TCP-port inspection/termination helpers.
//!
//! Used by `controller.rs` to answer two questions when the gateway's
//! configured port fails to bind: "who currently holds this port?" (via
//! [`get_port_occupier`]), and "is it safe for us to kill that process
//! automatically?" (via the self-owned-process check inside
//! [`terminate_process`]). All process introspection here shells out to
//! platform-native tools (`lsof`/`ps` on Unix, `netstat`/`tasklist`/`wmic`
//! on Windows) rather than using a cross-platform process-listing library,
//! since the exact tool availability and output format is well-understood
//! and stable on each OS.

#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::process::Command;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Identifies whatever process is currently listening on a given TCP port.
#[derive(Debug, Clone)]
pub struct PortOccupier {
    pub pid: u32,
    pub process_name: String,
}

/// Fetches the full command line a process was launched with, via `ps -o
/// command=` on Unix. Returns an empty string if the process no longer
/// exists or the command fails, rather than propagating an error, since
/// callers use this only for display/heuristic purposes.
#[cfg(unix)]
fn get_process_command(pid: u32) -> String {
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output();
    if let Ok(out) = output {
        if out.status.success() {
            return String::from_utf8_lossy(&out.stdout).trim().to_string();
        }
    }
    String::new()
}

/// Windows equivalent of the Unix `get_process_command`, using `wmic
/// process where ProcessId=<pid> get CommandLine`. Spawned with
/// `CREATE_NO_WINDOW` so no console flash appears when this runs from the
/// GUI process.
#[cfg(windows)]
fn get_process_command(pid: u32) -> String {
    let output = Command::new("wmic")
        .args([
            "process",
            "where",
            &format!("ProcessId={}", pid),
            "get",
            "CommandLine",
            "/value",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    if let Ok(out) = output {
        if out.status.success() {
            let text = String::from_utf8_lossy(&out.stdout);
            for line in text.lines() {
                if let Some(rest) = line.strip_prefix("CommandLine=") {
                    return rest.trim().to_string();
                }
            }
        }
    }
    String::new()
}

/// Fetches just the process's short executable name (not the full command
/// line) via `ps -o comm=`.
#[cfg(unix)]
fn get_process_name(pid: u32) -> String {
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output();
    match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => String::new(),
    }
}

/// Windows equivalent of the Unix `get_process_name`, parsing the first CSV
/// row emitted by `tasklist /FI "PID eq <pid>" /FO CSV /NH` to pull out the
/// quoted image name field.
#[cfg(windows)]
fn get_process_name(pid: u32) -> String {
    let output = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {}", pid), "/FO", "CSV", "/NH"])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    match output {
        Ok(o) if o.status.success() => {
            let row = String::from_utf8_lossy(&o.stdout);
            let first = row.lines().next().unwrap_or("").trim();
            first
                .strip_prefix('"')
                .and_then(|s| s.split('"').next())
                .unwrap_or("")
                .to_string()
        }
        _ => String::new(),
    }
}

/// Looks up which process, if any, is currently listening on `port` (TCP),
/// returning `Ok(None)` if the port is free.
///
/// On Unix, shells out to `lsof -nP -iTCP:<port> -sTCP:LISTEN -Fpc`, whose
/// `-F` output format prefixes each field with a single letter (`p` for
/// PID, `c` for command name) on its own line; this scans those lines,
/// taking the *first* `p`/`c` value seen (a listening socket can have
/// multiple matching lines, e.g. one per address family, but they all
/// belong to the same process). A non-zero exit status from `lsof` (which
/// happens whenever nothing matches the filter) is treated as "port free",
/// not an error.
///
/// On Windows, parses `netstat -ano -p tcp` output, matching lines that
/// contain `LISTENING` and a local-address column ending in `:<port>`, then
/// takes the PID from the last whitespace-separated column (netstat's fixed
/// column layout puts PID last).
#[allow(clippy::needless_return)]
pub fn get_port_occupier(port: u16) -> Result<Option<PortOccupier>, String> {
    #[cfg(unix)]
    {
        let output = Command::new("lsof")
            .args(["-nP", &format!("-iTCP:{}", port), "-sTCP:LISTEN", "-Fpc"])
            .output()
            .map_err(|e| format!("Failed to execute lsof: {}", e))?;

        if !output.status.success() {
            return Ok(None);
        }

        let text = String::from_utf8_lossy(&output.stdout);
        let mut pid: Option<u32> = None;
        let mut process_name = String::new();

        // `lsof -F` prefixes each output line with a single-letter field
        // marker (no separator) rather than emitting structured records; we
        // just accept the first PID and first command name we encounter.
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix('p') {
                if pid.is_none() {
                    pid = rest.trim().parse::<u32>().ok();
                }
            } else if let Some(rest) = line.strip_prefix('c') {
                if process_name.is_empty() {
                    process_name = rest.trim().to_string();
                }
            }
            if pid.is_some() && !process_name.is_empty() {
                break;
            }
        }

        if let Some(found_pid) = pid {
            return Ok(Some(PortOccupier {
                pid: found_pid,
                process_name,
            }));
        }

        return Ok(None);
    }

    #[cfg(windows)]
    {
        let output = Command::new("netstat")
            .args(["-ano", "-p", "tcp"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map_err(|e| format!("Failed to execute netstat: {}", e))?;

        if !output.status.success() {
            return Ok(None);
        }

        let text = String::from_utf8_lossy(&output.stdout);
        let needle = format!(":{}", port);
        let mut found_pid: Option<u32> = None;

        // netstat's fixed columns are Proto / Local Address / Foreign
        // Address / State / PID, so the PID is always the last
        // whitespace-separated token on a matching line.
        for line in text.lines() {
            if !line.contains("LISTENING") || !line.contains(&needle) {
                continue;
            }
            let cols: Vec<&str> = line.split_whitespace().collect();
            if let Some(pid_col) = cols.last() {
                found_pid = pid_col.parse::<u32>().ok();
                if found_pid.is_some() {
                    break;
                }
            }
        }

        if let Some(pid) = found_pid {
            return Ok(Some(PortOccupier {
                pid,
                process_name: get_process_name(pid),
            }));
        }

        return Ok(None);
    }
}

/// Executable name "stems" that identify a process as belonging to Lanius
/// itself (as opposed to some unrelated process that happens to occupy the
/// configured port), checked case-insensitively and without path/extension.
const SELF_OWNED_PROCESS_STEMS: &[&str] = &["lanius-desktop", "lanius"];

/// Normalizes a raw process name or executable path down to a comparable
/// "stem": strips any directory component (splitting on both `/` and `\`
/// so this works for paths from either Unix or Windows tools), lowercases
/// it, and strips a trailing `.exe` if present. This lets
/// `is_self_owned_process` compare `"Lanius"`, `"lanius.exe"`,
/// `"/opt/lanius/bin/lanius"`, and `"C:\...\lanius.exe"` as all equal to the
/// stem `"lanius"`.
fn normalize_process_stem(raw: &str) -> String {
    let trimmed = raw.trim();
    let base = trimmed.rsplit(['/', '\\']).next().unwrap_or(trimmed);
    let lower = base.to_ascii_lowercase();
    lower.strip_suffix(".exe").unwrap_or(&lower).to_string()
}

/// Decides whether a process (identified by its short `name` and/or full
/// `command` line) is "ours" — i.e. safe to terminate automatically
/// without explicit user confirmation.
///
/// This is a security-relevant heuristic: it must be permissive enough to
/// recognize legitimate stale instances of Lanius/lanius-desktop (so a
/// restart can actually self-heal after a crash or previous exit), but strict
/// enough to refuse killing an unrelated process that happens to be
/// listening on the configured port. Three signals are checked, any one of
/// which is sufficient: the process's own short name matches a known stem,
/// the first token of its command line (the invoked executable) matches a
/// known stem, or the command line references a path inside a
/// `Lanius.app/` bundle (covering the case where `name`/the first argv
/// token doesn't directly reveal the executable, e.g. a wrapper script).
/// Lookalike names (`my-lanius`, `laniusd`, `lanius.sh`, etc.) are
/// deliberately *not* matched — only an exact stem match counts.
fn is_self_owned_process(name: &str, command: &str) -> bool {
    if SELF_OWNED_PROCESS_STEMS.contains(&normalize_process_stem(name).as_str()) {
        return true;
    }

    if let Some(exe) = command.split_whitespace().next() {
        if SELF_OWNED_PROCESS_STEMS.contains(&normalize_process_stem(exe).as_str()) {
            return true;
        }
    }

    command.to_ascii_lowercase().contains("lanius.app/")
}

/// Terminates the process identified by `pid`.
///
/// Safety gate: refuses to terminate the current process (comparing against
/// `std::process::id()`), and — unless `force` is `true` — refuses to
/// terminate any process that [`is_self_owned_process`] does not recognize
/// as belonging to Lanius, returning a descriptive error instead of acting.
/// `force: true` would only be used after the user explicitly confirmed
/// killing a specific, displayed process; the only current caller
/// (`controller.rs`'s stale-port cleanup before starting the gateway)
/// always calls this with `force: false`.
///
/// On Unix this sends `SIGTERM` via `libc::kill` (an `unsafe` FFI call, but
/// one with no memory-safety hazard — it only affects OS process state); on
/// Windows it shells out to `taskkill /F /PID <pid>`.
#[allow(clippy::needless_return)]
pub fn terminate_process(pid: u32, force: bool) -> Result<(), String> {
    if pid == std::process::id() {
        return Err(format!(
            "Refusing to terminate PID {pid}: that is this application itself."
        ));
    }

    if !force {
        let name = get_process_name(pid);
        let command = get_process_command(pid);
        if !is_self_owned_process(&name, &command) {
            let shown = if name.is_empty() {
                "<unknown>".to_string()
            } else {
                name
            };
            return Err(format!(
                "Refusing to auto-terminate PID {pid} ({shown}): not a Lanius process. \
                 Change the port in settings, or stop that process yourself."
            ));
        }
    }

    #[cfg(unix)]
    {
        unsafe {
            if libc::kill(pid as i32, libc::SIGTERM) != 0 {
                return Err(format!("Failed to terminate process {} with SIGTERM", pid));
            }
        }
        return Ok(());
    }

    #[cfg(windows)]
    {
        let output = Command::new("taskkill")
            .args(["/F", "/PID", &pid.to_string()])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map_err(|e| format!("Failed to execute taskkill: {}", e))?;

        if output.status.success() {
            return Ok(());
        }

        return Err(format!("Failed to terminate process {}", pid));
    }
}

#[cfg(test)]
mod tests {
    use super::{is_self_owned_process, normalize_process_stem};

    #[test]
    fn stem_normalization_handles_paths_case_and_exe() {
        assert_eq!(normalize_process_stem("Lanius"), "lanius");
        assert_eq!(normalize_process_stem("lanius.exe"), "lanius");
        assert_eq!(normalize_process_stem("LANIUS.EXE"), "lanius");
        assert_eq!(normalize_process_stem("/opt/lanius/bin/lanius"), "lanius");
        assert_eq!(
            normalize_process_stem("C:\\Program Files\\Lanius\\lanius.exe"),
            "lanius"
        );
        assert_eq!(normalize_process_stem("  lanius  "), "lanius");
        assert_eq!(
            normalize_process_stem("lanius-desktop.exe"),
            "lanius-desktop"
        );
    }

    #[test]
    fn recognizes_our_own_processes() {
        for (name, command) in [
            ("lanius", ""),
            ("Lanius", ""),
            ("lanius.exe", ""),
            ("/opt/lanius/bin/lanius", ""),
            ("lanius-desktop", ""),
            ("Lanius-Desktop", ""),
            ("lanius-desktop.exe", ""),
            ("", "/usr/local/bin/lanius serve"),
            ("Lanius", "/Applications/Lanius.app/Contents/MacOS/Lanius"),
        ] {
            assert!(
                is_self_owned_process(name, command),
                "should be recognized as ours: name={name:?} command={command:?}"
            );
        }
    }

    #[test]
    fn refuses_foreign_and_lookalike_processes() {
        for (name, command) in [
            ("", ""),
            ("node", "node server.js"),
            ("python3", "python3 -m http.server 8080"),
            ("nginx", "nginx: master process"),
            ("lanius-experiments", ""),
            ("my-lanius", ""),
            ("laniusd", ""),
            ("laniustest", ""),
            ("xlanius", ""),
            ("lanius.sh", ""),
            ("lanius-helper", ""),
            ("lanius-desktop-helper", ""),
            (
                "Code Helper",
                "/Applications/Visual Studio Code.app/Contents/MacOS/Code",
            ),
            ("evil", "/tmp/lanius.app.evil/payload"),
        ] {
            assert!(
                !is_self_owned_process(name, command),
                "must NOT be treated as ours: name={name:?} command={command:?}"
            );
        }
    }

    #[test]
    fn empty_identity_is_refused() {
        assert!(!is_self_owned_process("", ""));
    }

    #[test]
    fn never_terminates_this_process() {
        let own = std::process::id();
        for force in [false, true] {
            let err = super::terminate_process(own, force)
                .expect_err("terminating ourselves must be refused");
            assert!(
                err.contains("this application itself"),
                "unexpected error: {err}"
            );
        }
    }
}
