use crate::config::Config;
use anyhow::{bail, Context, Result};
use nix::{
    errno::Errno,
    fcntl::{Flock, FlockArg},
    sys::signal::{kill, Signal},
    unistd::{geteuid, Pid},
};
use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    net::TcpStream,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    thread::sleep,
    time::{Duration, Instant},
};

pub const DAEMON_CHILD_ENV: &str = "INDEX_TRANSLATE_DAEMON_CHILD";

/// Directory for the log and (when no runtime dir exists) the pid file.
pub fn default_state_dir() -> PathBuf {
    if let Some(dir) = env::var_os("XDG_STATE_HOME").map(PathBuf::from) {
        return dir.join("index-translate");
    }
    if let Some(home) = env::var_os("HOME").map(PathBuf::from) {
        return home.join(".local/state/index-translate");
    }
    env::temp_dir().join(format!("index-translate-{}", geteuid().as_raw()))
}

pub fn default_pid_file() -> PathBuf {
    if let Some(dir) = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_dir())
    {
        return dir.join("index-translate.pid");
    }
    default_state_dir().join("index-translate.pid")
}

pub fn default_log_file() -> PathBuf {
    default_state_dir().join("index-translate.log")
}

/// Owns an exclusively locked pid file for the lifetime of the process.
pub struct PidGuard {
    _lock: Flock<File>,
    path: PathBuf,
    pid: u32,
}

impl PidGuard {
    pub fn acquire(path: &Path) -> Result<Self> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            ensure_private_dir(parent)?;
        }

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o644)
            .open(path)
            .with_context(|| format!("failed to open pid file {}", path.display()))?;

        let mut lock = match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(lock) => lock,
            Err((mut file, Errno::EWOULDBLOCK)) => {
                let mut existing = String::new();
                let _ = file.read_to_string(&mut existing);
                let existing = existing.trim();
                if existing.is_empty() {
                    bail!("another index-translate instance is already running");
                }
                bail!("another index-translate instance is already running (pid {existing})");
            }
            Err((_file, error)) => return Err(error).context("failed to lock pid file"),
        };

        let pid = std::process::id();
        let file = &mut *lock;
        file.set_len(0).context("failed to truncate pid file")?;
        file.seek(SeekFrom::Start(0))
            .context("failed to rewind pid file")?;
        writeln!(file, "{pid}").context("failed to write pid file")?;
        file.flush().context("failed to flush pid file")?;
        let _ = file.sync_data();

        Ok(Self {
            _lock: lock,
            path: path.to_path_buf(),
            pid,
        })
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }
}

impl Drop for PidGuard {
    fn drop(&mut self) {
        let pid_text = self.pid.to_string();
        let current = fs::read_to_string(&self.path).ok();
        if current.as_deref().map(str::trim) == Some(pid_text.as_str()) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub fn read_pid(path: &Path) -> Option<u32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

pub fn process_alive(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    match kill(Pid::from_raw(pid as i32), None) {
        Ok(()) => !is_zombie(pid),
        Err(Errno::EPERM) => true,
        Err(_) => false,
    }
}

/// Guard against PID reuse while accepting another copy of this binary:
/// compare the executable name rather than its full path, because a service
/// installed to `~/.local/bin` may be inspected by `target/release/index-translate`.
/// When /proc cannot be read, assume the pid file is right.
pub fn is_our_process(pid: u32) -> bool {
    if !process_alive(pid) {
        return false;
    }
    let Some(current_name) = env::current_exe()
        .ok()
        .and_then(|path| path.file_name().map(|name| name.to_owned()))
    else {
        return true;
    };

    let executable_name = fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .and_then(|path| path.file_name().map(|name| name.to_owned()));
    if executable_name.as_ref() == Some(&current_name) {
        return true;
    }

    let command_line = fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    let argv0 = command_line
        .split(|byte| *byte == 0)
        .next()
        .unwrap_or_default();
    match std::str::from_utf8(argv0)
        .ok()
        .and_then(|argv0| Path::new(argv0).file_name())
    {
        Some(name) => name == current_name.as_os_str(),
        None => true,
    }
}

/// Send SIGTERM and wait for the daemon to release its pid file and exit.
/// Returns the stopped pid, or `None` when nothing was running.
pub fn stop(pid_file: &Path, timeout: Duration) -> Result<Option<u32>> {
    let Some(pid) = read_pid(pid_file) else {
        if pid_file.exists() {
            let _ = fs::remove_file(pid_file);
        }
        return Ok(None);
    };

    if !is_our_process(pid) {
        let _ = fs::remove_file(pid_file);
        return Ok(None);
    }

    match kill(Pid::from_raw(pid as i32), Signal::SIGTERM) {
        Ok(()) | Err(Errno::ESRCH) => {}
        Err(error) => return Err(error).context("failed to send SIGTERM"),
    }

    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !process_alive(pid) {
            let _ = fs::remove_file(pid_file);
            return Ok(Some(pid));
        }
        sleep(Duration::from_millis(100));
    }

    bail!(
        "index-translate (pid {pid}) did not stop within {}s",
        timeout.as_secs()
    )
}

fn is_zombie(pid: u32) -> bool {
    let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    stat.rsplit_once(')')
        .map(|(_, rest)| rest.trim_start().starts_with('Z'))
        .unwrap_or(false)
}

fn ensure_private_dir(path: &Path) -> Result<()> {
    if path.is_dir() {
        return Ok(());
    }
    fs::create_dir_all(path)
        .with_context(|| format!("failed to create directory {}", path.display()))?;
    // Only tighten permissions on the directory we just created.
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
    Ok(())
}

/// Probe the proxy's local `/health` endpoint. Used by `start` and service installs.
pub fn health_probe(config: &Config) -> bool {
    let Ok(addr) = config.connect_addr() else {
        return false;
    };
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(500)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
    if stream
        .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .is_err()
    {
        return false;
    }
    let mut response = String::new();
    if stream.read_to_string(&mut response).is_err() {
        return false;
    }
    (response.starts_with("HTTP/1.1 200") || response.starts_with("HTTP/1.0 200"))
        && response.contains("\"service\":\"index-translate\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_path(name: &str) -> PathBuf {
        let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
        env::temp_dir().join(format!(
            "index-translate-test-{}-{suffix}-{name}",
            std::process::id()
        ))
    }

    #[test]
    fn pid_file_roundtrip_and_cleanup() {
        let path = temp_path("pid");
        {
            let guard = PidGuard::acquire(&path).unwrap();
            assert_eq!(read_pid(&path), Some(guard.pid()));
        }
        assert!(!path.exists());
    }

    #[test]
    fn second_guard_is_rejected() {
        let path = temp_path("locked.pid");
        let _guard = PidGuard::acquire(&path).unwrap();
        let error = match PidGuard::acquire(&path) {
            Ok(_) => panic!("second pid lock unexpectedly succeeded"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("already running"), "{error}");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn stale_pid_is_not_ours() {
        assert!(!process_alive(u32::MAX));
        assert!(!is_our_process(u32::MAX));
    }

    #[test]
    fn recognizes_current_process() {
        assert!(is_our_process(std::process::id()));
    }
}
