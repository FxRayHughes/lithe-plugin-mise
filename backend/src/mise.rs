//! Locates and runs `mise`. A child runs in its own process group so a kill
//! also reaches anything it started.

use std::ffi::OsStr;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const OUTPUT_LIMIT: u64 = 32 * 1024 * 1024;
const SIGKILL: i32 = 9;

extern "C" {
    fn kill(pid: i32, signal: i32) -> i32;
}

pub struct Options {
    /// The user's `misePath` setting; an invalid value never falls back.
    pub configured: Option<PathBuf>,
    pub offline: bool,
    pub child_timeout: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            configured: None,
            offline: true,
            child_timeout: Duration::from_secs(4),
        }
    }
}

#[derive(Debug)]
pub enum RunError {
    NotFound,
    Cancelled,
    Timeout,
    Failed(String),
}

fn is_executable(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// Setting, then `~/.local/bin/mise`, Homebrew, and finally `PATH`.
pub fn locate(options: &Options) -> Option<PathBuf> {
    if let Some(path) = &options.configured {
        return (path.is_absolute() && is_executable(path)).then(|| path.clone());
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let fixed = home
        .iter()
        .map(|home| home.join(".local/bin/mise"))
        .chain(["/opt/homebrew/bin/mise", "/usr/local/bin/mise"].map(PathBuf::from));
    let path = std::env::var_os("PATH").unwrap_or_default();
    let searched: Vec<PathBuf> = std::env::split_paths(&path)
        .filter(|directory| directory.is_absolute())
        .map(|directory| directory.join("mise"))
        .collect();
    fixed.chain(searched).find(|path| is_executable(path))
}

/// Runs one bounded child and returns its stdout. Cancellation and the deadline
/// kill the whole process group.
pub fn run(
    program: &Path,
    arguments: &[&OsStr],
    directory: &Path,
    options: &Options,
    cancelled: &AtomicBool,
) -> Result<Vec<u8>, RunError> {
    let mut command = Command::new(program);
    command
        .args(arguments)
        .current_dir(directory)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    if options.offline {
        command.env("MISE_OFFLINE", "1");
    }
    let mut child = command.spawn().map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied => RunError::NotFound,
        _ => RunError::Failed(error.to_string()),
    })?;
    let group = child.id() as i32;
    let collect = |stream: Box<dyn Read + Send>| {
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = stream.take(OUTPUT_LIMIT).read_to_end(&mut bytes);
            let _ = sender.send(bytes);
        });
        receiver
    };
    let stdout = collect(Box::new(child.stdout.take().expect("stdout is piped")));
    let stderr = collect(Box::new(child.stderr.take().expect("stderr is piped")));
    let deadline = Instant::now() + options.child_timeout;
    let stop = |child: &mut std::process::Child| {
        // SAFETY: `kill` takes plain integers; the group belongs to this unreaped child.
        unsafe { kill(-group, SIGKILL) };
        let _ = child.wait();
    };
    let status = loop {
        if cancelled.load(Ordering::SeqCst) {
            stop(&mut child);
            return Err(RunError::Cancelled);
        }
        if Instant::now() >= deadline {
            stop(&mut child);
            return Err(RunError::Timeout);
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(error) => {
                stop(&mut child);
                return Err(RunError::Failed(error.to_string()));
            }
        }
    };
    // A descendant that left the group may hold a pipe open; never wait on it forever.
    let grace = Duration::from_secs(1);
    let error_text = stderr.recv_timeout(grace).unwrap_or_default();
    let Ok(output) = stdout.recv_timeout(grace) else {
        return Err(RunError::Failed("mise left its output open".to_owned()));
    };
    if status.success() {
        Ok(output)
    } else {
        Err(RunError::Failed(
            String::from_utf8_lossy(&error_text).into_owned(),
        ))
    }
}
