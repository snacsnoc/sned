//! PTY-backed command execution for programs that require a terminal.
//!
//! Programs that check `isatty()` (npm install, ssh, docker, top, ipython)
//! behave differently or fail when stdout is not a terminal. This module
//! spawns commands inside a pseudo-terminal so they see a real tty.
//!
//! ## Design
//!
//! - Uses `nix::pty::forkpty()` to create a PTY and fork in one call.
//! - The child process sets up the PTY slave as stdin/stdout/stderr and execs.
//! - The parent reads from the master fd and waits for the child.
//! - Process group is used for timeout-based killing.

use std::ffi::CString;
use std::io::{self, Read};
use std::path::Path;
use std::time::Duration;

use nix::fcntl::{FcntlArg, OFlag, fcntl};
use nix::pty::{ForkptyResult, PtyMaster, Winsize};
use nix::sys::wait::WaitPidFlag;
use nix::unistd::{self, Pid};

use crate::terminal::strip_progress_artifacts;

/// Output from a PTY-backed command execution.
#[derive(Debug, Clone)]
pub struct CommandOutput {
    pub stdout: String,
    pub exit_code: i32,
    pub timed_out: bool,
}

/// Run a command inside a PTY with a timeout.
///
/// The command is executed as `sh -c "<cmd>"` inside the PTY.
/// On timeout, the entire process group is killed with SIGKILL.
pub fn run_command_in_pty(
    cmd: &str,
    cwd: Option<&Path>,
    rows: u16,
    cols: u16,
    timeout: Duration,
) -> Result<CommandOutput, PtyError> {
    let winsize = Winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };

    // forkpty creates a PTY and forks in one call
    let result = unsafe { nix::pty::forkpty(Some(&winsize), None) };

    match result {
        Ok(ForkptyResult::Child) => {
            // Child process: exec the command
            child_exec(cmd, cwd);
            // unreachable!() - child_exec() calls execvp() which only returns on failure
            unreachable!("child_exec should not return");
        }
        Ok(ForkptyResult::Parent { child, master }) => {
            // Wrap the OwnedFd in a PtyMaster
            let master = unsafe { PtyMaster::from_owned_fd(master) };
            // Parent process: read from master and wait for child
            parent_read_and_wait(master, child, timeout)
        }
        Err(e) => Err(PtyError::ForkPtyFailed(e.to_string())),
    }
}

/// Child process exec.
fn child_exec(cmd: &str, cwd: Option<&Path>) {
    // Change working directory if requested
    if let Some(dir) = cwd {
        let _ = unistd::chdir(dir);
    }

    // Exec sh -c "<cmd>"
    let shell = CString::new("sh").unwrap();
    let flag = CString::new("-c").unwrap();
    let cmd_cstr = match CString::new(cmd) {
        Ok(s) => s,
        Err(_) => std::process::exit(127),
    };

    let _ = unistd::execvp(
        &shell,
        &[shell.as_c_str(), flag.as_c_str(), cmd_cstr.as_c_str()],
    );

    // If exec fails
    std::process::exit(127);
}

/// Parent process: read from master fd and wait for child.
fn parent_read_and_wait(
    mut master: PtyMaster,
    child: Pid,
    timeout: Duration,
) -> Result<CommandOutput, PtyError> {
    let start = std::time::Instant::now();
    let mut output = Vec::new();
    let mut buf = [0u8; 4096];

    // Set master to non-blocking mode
    let flags = fcntl(&master, FcntlArg::F_GETFL)?;
    let new_flags = OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK;
    fcntl(&master, FcntlArg::F_SETFL(new_flags))?;

    let mut timed_out = false;
    let mut exit_code = 0;

    loop {
        if start.elapsed() >= timeout {
            timed_out = true;
            // Kill the entire process group
            let _ = unsafe { libc::kill(-child.as_raw(), libc::SIGKILL) };
            // Wait for the child to be reaped
            let _ = nix::sys::wait::waitpid(child, None);
            break;
        }

        // Try to read from master
        match master.read(&mut buf) {
            Ok(0) => {
                // EOF - child has closed the PTY
                break;
            }
            Ok(n) => {
                output.extend_from_slice(&buf[..n]);
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                // No data available, check if child has exited
                match nix::sys::wait::waitpid(child, Some(WaitPidFlag::WNOHANG)) {
                    Ok(nix::sys::wait::WaitStatus::Exited(_, status)) => {
                        exit_code = status;
                        // Drain any remaining output
                        loop {
                            match master.read(&mut buf) {
                                Ok(0) => break,
                                Ok(n) => output.extend_from_slice(&buf[..n]),
                                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => break,
                                Err(_) => break,
                            }
                        }
                        break;
                    }
                    Ok(nix::sys::wait::WaitStatus::Signaled(_, sig, _)) => {
                        exit_code = 128 + sig as i32;
                        break;
                    }
                    Ok(_) => {
                        // Still running, sleep briefly
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => {
                        // Child may have already been reaped
                        break;
                    }
                }
            }
            Err(_) => {
                break;
            }
        }
    }

    // If we haven't gotten an exit code yet, try to wait for the child
    if exit_code == 0
        && !timed_out
        && let Ok(nix::sys::wait::WaitStatus::Exited(_, status)) =
            nix::sys::wait::waitpid(child, Some(WaitPidFlag::WNOHANG))
    {
        exit_code = status;
    }

    // Convert output to string, replacing invalid UTF-8
    let raw_stdout = String::from_utf8_lossy(&output).to_string();
    // Strip progress bar artifacts (repeated lines with percentages, spinners, etc.)
    let stdout = strip_progress_artifacts(&raw_stdout);

    Ok(CommandOutput {
        stdout,
        exit_code,
        timed_out,
    })
}

/// Errors that can occur during PTY operations.
#[derive(Debug, thiserror::Error)]
pub enum PtyError {
    #[error("PTY operation failed: {0}")]
    Nix(#[from] nix::Error),
    #[error("forkpty failed: {0}")]
    ForkPtyFailed(String),
    #[error("IO error: {0}")]
    Io(#[from] io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_run_command_in_pty_basic() {
        let result = run_command_in_pty("echo hello", None, 24, 80, Duration::from_secs(5));
        assert!(result.is_ok(), "PTY command should succeed: {:?}", result);
        let output = result.unwrap();
        assert!(
            output.stdout.contains("hello"),
            "Output should contain 'hello': {:?}",
            output.stdout
        );
        assert_eq!(output.exit_code, 0);
        assert!(!output.timed_out);
    }

    #[test]
    fn test_run_command_in_pty_timeout() {
        let result = run_command_in_pty("sleep 100", None, 24, 80, Duration::from_millis(500));
        assert!(result.is_ok(), "PTY timeout should not error: {:?}", result);
        let output = result.unwrap();
        assert!(output.timed_out, "Command should have timed out");
    }

    #[test]
    fn test_run_command_in_pty_with_cwd() {
        let tmp = std::env::temp_dir();
        let result = run_command_in_pty("pwd", Some(&tmp), 24, 80, Duration::from_secs(5));
        assert!(result.is_ok());
        let output = result.unwrap();
        // pwd output should be a valid path (not empty, not an error)
        let pwd_output = output.stdout.trim();
        assert!(!pwd_output.is_empty(), "pwd output should not be empty");
        // On macOS, temp_dir may return /var/... but pwd returns /private/var/...
        // Just verify the output looks like a path
        assert!(
            pwd_output.starts_with('/'),
            "pwd output should be an absolute path: {:?}",
            pwd_output
        );
    }

    #[test]
    fn test_run_command_in_pty_exit_code() {
        let result = run_command_in_pty("exit 42", None, 24, 80, Duration::from_secs(5));
        assert!(result.is_ok());
        let output = result.unwrap();
        assert_eq!(output.exit_code, 42);
    }

    #[test]
    fn test_run_command_in_pty_strips_progress_bars() {
        // Simulate a command that outputs progress bars
        // The PTY will echo the command and show the output
        let cmd = r#"printf "Downloading 50%%\rDownloading 100%%\nDone\n""#;
        let result = run_command_in_pty(cmd, None, 24, 80, Duration::from_secs(5));
        assert!(result.is_ok(), "PTY command should succeed: {:?}", result);
        let output = result.unwrap();
        // Progress lines should be stripped, only "Done" should remain
        assert!(
            output.stdout.contains("Done"),
            "Output should contain 'Done': {:?}",
            output.stdout
        );
        // The progress lines should be stripped
        assert!(
            !output.stdout.contains("50%"),
            "Output should not contain '50%' progress: {:?}",
            output.stdout
        );
        assert!(
            !output.stdout.contains("100%"),
            "Output should not contain '100%' progress: {:?}",
            output.stdout
        );
    }
}
