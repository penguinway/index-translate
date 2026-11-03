use crate::{
    config::Config,
    daemon::{self, health_probe},
};
use anyhow::{bail, Context, Result};
use nix::unistd::geteuid;
use std::{
    env, fs,
    io::ErrorKind,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
    thread::sleep,
    time::{Duration, Instant},
};

const UNIT_NAME: &str = "index-translate.service";
const MIB: usize = 1024 * 1024;
const READY_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    User,
    System,
}

impl Scope {
    fn label(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::System => "system",
        }
    }

    fn wanted_by(self) -> &'static str {
        match self {
            Self::User => "default.target",
            Self::System => "multi-user.target",
        }
    }

    fn systemctl_label(self) -> &'static str {
        match self {
            Self::User => "systemctl --user",
            Self::System => "systemctl",
        }
    }

    fn journalctl_label(self) -> &'static str {
        match self {
            Self::User => "journalctl --user",
            Self::System => "journalctl",
        }
    }

    fn command(self) -> Command {
        let mut command = Command::new("systemctl");
        if self == Self::User {
            command.arg("--user");
        }
        command
    }
}

pub struct InstallOptions<'a> {
    pub scope: Scope,
    pub config: &'a Config,
    pub pid_file: Option<&'a Path>,
    pub log_level: &'a str,
    pub linger: bool,
}

/// Copy the binary to a stable location, write a systemd unit, then
/// enable and restart the service. A manually started daemon is stopped first
/// so the PID lock and listening port are free.
pub fn install(options: &InstallOptions<'_>) -> Result<()> {
    validate(options.scope, options.linger)?;
    ensure_systemd(options.scope)?;

    let pid_file = resolve_pid_file(options.scope, options.pid_file)?;
    let unit_path = unit_path(options.scope)?;
    let executable = install_binary(options.scope)?;
    let unit = render_unit(
        &executable,
        options.config,
        options.scope,
        &pid_file,
        options.log_level,
    )?;
    write_unit(&unit_path, &unit)?;

    run_systemctl(options.scope, &["daemon-reload"])?;
    stop_existing(options.scope, &pid_file)?;
    run_systemctl(options.scope, &["enable", UNIT_NAME])?;
    run_systemctl(options.scope, &["restart", UNIT_NAME])?;
    wait_ready(options.config, options.scope)?;

    if options.linger {
        enable_linger();
    }

    println!(
        "已安装并启动 systemd {} 服务: {UNIT_NAME}",
        options.scope.label()
    );
    println!("  单元文件 (Unit): {}", unit_path.display());
    println!("  可执行文件 (Binary): {}", executable.display());
    println!("  接口地址 (API URL): {}", options.config.endpoint());
    println!("  PID 文件: {}", pid_file.display());
    println!(
        "  状态 (Status): {} status {UNIT_NAME}",
        options.scope.systemctl_label()
    );
    println!(
        "  日志 (Logs): {} -u {UNIT_NAME} -f",
        options.scope.journalctl_label()
    );
    match options.scope {
        Scope::User => println!("  卸载 (Remove): index-translate uninstall-service"),
        Scope::System => {
            println!("  卸载 (Remove): sudo index-translate uninstall-service --system")
        }
    }
    println!("注意: 已安装的服务与 `index-translate start` 不要同时使用。");

    Ok(())
}

pub fn uninstall(scope: Scope) -> Result<()> {
    if scope == Scope::System && !geteuid().is_root() {
        bail!("--system requires root; rerun with sudo");
    }
    ensure_systemd(scope)?;

    let path = unit_path(scope)?;
    if !path.exists() {
        println!(
            "未安装 systemd {} 服务 (unit 不存在: {})",
            scope.label(),
            path.display()
        );
        return Ok(());
    }

    systemctl_ignore(scope, &["stop", UNIT_NAME]);
    systemctl_ignore(scope, &["disable", UNIT_NAME]);
    fs::remove_file(&path).with_context(|| format!("failed to remove unit {}", path.display()))?;
    systemctl_ignore(scope, &["daemon-reload"]);
    systemctl_ignore(scope, &["reset-failed", UNIT_NAME]);

    println!("已移除 systemd {} 服务: {UNIT_NAME}", scope.label());
    println!("可执行文件和日志已保留。");
    Ok(())
}

fn validate(scope: Scope, linger: bool) -> Result<()> {
    if scope == Scope::System && !geteuid().is_root() {
        bail!("--system requires root; rerun with sudo");
    }
    if scope == Scope::System && linger {
        bail!("--linger only applies to user services");
    }
    Ok(())
}

fn ensure_systemd(scope: Scope) -> Result<()> {
    match Command::new("systemctl").arg("--version").output() {
        Ok(output) if output.status.success() => {}
        Ok(output) => bail!(
            "systemctl is not usable: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
        Err(error) if error.kind() == ErrorKind::NotFound => {
            bail!("systemctl not found; the service installer requires systemd")
        }
        Err(error) => return Err(error).context("failed to run systemctl"),
    }

    if scope == Scope::User {
        let output = scope
            .command()
            .args(["show-environment"])
            .output()
            .context("failed to run systemctl --user")?;
        if !output.status.success() {
            bail!(
                "systemd user manager is not reachable: {}\n\
                 Run this from a desktop login session, or use `sudo index-translate install-service --system`.",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
    }
    Ok(())
}

fn install_binary(scope: Scope) -> Result<PathBuf> {
    let current = env::current_exe().context("failed to locate the current executable")?;
    let target = binary_path(scope)?;
    if same_file(&current, &target) {
        return Ok(target);
    }

    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let temporary = target.with_file_name(format!(".index-translate.{}.tmp", std::process::id()));
    let result = (|| -> Result<()> {
        fs::copy(&current, &temporary)
            .with_context(|| format!("failed to copy binary to {}", temporary.display()))?;
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o755))
            .context("failed to mark installed binary executable")?;
        fs::rename(&temporary, &target)
            .with_context(|| format!("failed to install binary to {}", target.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result?;

    println!("已安装可执行文件: {}", target.display());
    Ok(target)
}

fn write_unit(path: &Path, unit: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let temporary = path.with_file_name(format!(".{UNIT_NAME}.{}.tmp", std::process::id()));
    let result = (|| -> Result<()> {
        fs::write(&temporary, unit)
            .with_context(|| format!("failed to write {}", temporary.display()))?;
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o644))
            .context("failed to set unit permissions")?;
        fs::rename(&temporary, path)
            .with_context(|| format!("failed to install unit {}", path.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn render_unit(
    executable: &Path,
    config: &Config,
    scope: Scope,
    pid_file: &Path,
    log_level: &str,
) -> Result<String> {
    let mut arguments = vec![
        executable.display().to_string(),
        "serve".to_owned(),
        "--api-base".to_owned(),
        config.api_base.clone(),
        "--host".to_owned(),
        config.host.clone(),
        "--port".to_owned(),
        config.port.to_string(),
        "--model".to_owned(),
        config.model.clone(),
        "--max-body-mib".to_owned(),
        config.max_body_bytes.div_ceil(MIB).to_string(),
        "--log-level".to_owned(),
        log_level.to_owned(),
        "--pid-file".to_owned(),
        pid_file.display().to_string(),
    ];
    if config.access_log {
        arguments.push("--access-log".to_owned());
    }
    if arguments
        .iter()
        .any(|argument| argument.contains(['\n', '\r']))
    {
        bail!("service arguments must not contain newlines");
    }

    let exec_start = arguments
        .iter()
        .map(|argument| systemd_escape(argument))
        .collect::<Vec<_>>()
        .join(" ");

    let mut unit = String::new();
    unit.push_str("[Unit]\n");
    unit.push_str(
        "Description=Index-Translate local OpenAI-compatible proxy for Immersive Translate\n",
    );
    if scope == Scope::System {
        unit.push_str("After=network-online.target\nWants=network-online.target\n");
    }
    unit.push_str("\n[Service]\nType=simple\n");
    unit.push_str(&format!("ExecStart={exec_start}\n"));
    unit.push_str("Restart=on-failure\nRestartSec=3\nTimeoutStopSec=15\n");
    unit.push_str("\n[Install]\n");
    unit.push_str(&format!("WantedBy={}\n", scope.wanted_by()));
    Ok(unit)
}

fn resolve_pid_file(scope: Scope, explicit: Option<&Path>) -> Result<PathBuf> {
    let path = match explicit {
        Some(path) if path.is_absolute() => path.to_path_buf(),
        Some(path) => env::current_dir()
            .context("failed to resolve the current directory")?
            .join(path),
        None if scope == Scope::System => PathBuf::from("/run/index-translate.pid"),
        None => daemon::default_pid_file(),
    };
    Ok(path)
}

fn unit_path(scope: Scope) -> Result<PathBuf> {
    match scope {
        Scope::User => {
            let config_home = env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .unwrap_or(home_dir()?.join(".config"));
            Ok(config_home.join("systemd/user").join(UNIT_NAME))
        }
        Scope::System => Ok(PathBuf::from("/etc/systemd/system").join(UNIT_NAME)),
    }
}

fn binary_path(scope: Scope) -> Result<PathBuf> {
    match scope {
        Scope::User => Ok(home_dir()?.join(".local/bin/index-translate")),
        Scope::System => Ok(PathBuf::from("/usr/local/bin/index-translate")),
    }
}

fn home_dir() -> Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .context("HOME is not set")
}

fn same_file(left: &Path, right: &Path) -> bool {
    match (fs::canonicalize(left), fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

fn stop_existing(scope: Scope, pid_file: &Path) -> Result<()> {
    if service_active(scope)? {
        systemctl_ignore(scope, &["stop", UNIT_NAME]);
    }
    if let Some(pid) = daemon::stop(pid_file, Duration::from_secs(10))? {
        println!("已停止现有的后台进程 (pid {pid})。");
    }
    Ok(())
}

fn service_active(scope: Scope) -> Result<bool> {
    let status = scope
        .command()
        .args(["is-active", "--quiet", UNIT_NAME])
        .status()
        .context("failed to run systemctl is-active")?;
    Ok(status.success())
}

fn wait_ready(config: &Config, scope: Scope) -> Result<()> {
    let deadline = Instant::now() + READY_TIMEOUT;
    while Instant::now() < deadline {
        if health_probe(config) {
            return Ok(());
        }
        sleep(Duration::from_millis(200));
    }

    if let Ok(output) = scope
        .command()
        .args(["status", "--no-pager", "--lines=15", UNIT_NAME])
        .output()
    {
        let details = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        eprintln!("{}", details.trim());
    }
    bail!(
        "service did not become healthy within {}s; check `{} -u {UNIT_NAME} -f`",
        READY_TIMEOUT.as_secs(),
        scope.journalctl_label()
    )
}

fn run_systemctl(scope: Scope, arguments: &[&str]) -> Result<Output> {
    let output = scope
        .command()
        .args(arguments)
        .output()
        .with_context(|| format!("failed to run systemctl {}", arguments.join(" ")))?;
    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "`{} {}` failed:\n{}{}",
            scope.systemctl_label(),
            arguments.join(" "),
            stdout.trim(),
            stderr.trim()
        );
    }
    Ok(output)
}

fn systemctl_ignore(scope: Scope, arguments: &[&str]) {
    let _ = scope.command().args(arguments).output();
}

fn enable_linger() {
    match Command::new("loginctl").arg("enable-linger").status() {
        Ok(status) if status.success() => {
            println!("已启用 systemd linger，用户服务将随系统启动。");
        }
        _ => {
            eprintln!(
                "警告: 无法启用 lingering；服务仍会在下次登录时自动启动。\
                 可手动执行 `loginctl enable-linger $USER`。"
            );
        }
    }
}

/// Escape one ExecStart argument. `%` would be expanded as a systemd specifier
/// and `$` as an environment variable, so both are doubled; whitespace and
/// quotes force double-quoting.
fn systemd_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '%' => escaped.push_str("%%"),
            '$' => escaped.push_str("$$"),
            _ => escaped.push(character),
        }
    }

    let needs_quotes = escaped.is_empty()
        || escaped
            .chars()
            .any(|character| character.is_whitespace() || matches!(character, '"' | '\'' | '\\'));
    if needs_quotes {
        format!("\"{}\"", escaped.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        escaped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> Config {
        Config::new(
            "https://example.com/v1",
            "Index-Translate-35B-A3B",
            "127.0.0.1",
            18080,
            false,
            64 * MIB,
        )
        .unwrap()
    }

    #[test]
    fn renders_user_unit_with_current_configuration() {
        let unit = render_unit(
            Path::new("/home/user/.local/bin/index-translate"),
            &test_config(),
            Scope::User,
            Path::new("/run/user/1000/index-translate.pid"),
            "info",
        )
        .unwrap();

        assert!(unit.contains("WantedBy=default.target"));
        assert!(unit.contains("--port 18080"));
        assert!(unit.contains("--model Index-Translate-35B-A3B"));
        assert!(unit.contains("--pid-file /run/user/1000/index-translate.pid"));
        assert!(!unit.contains("After=network-online.target"));
        assert!(!unit.contains("--access-log"));
    }

    #[test]
    fn renders_system_unit_with_network_ordering() {
        let unit = render_unit(
            Path::new("/usr/local/bin/index-translate"),
            &test_config(),
            Scope::System,
            Path::new("/run/index-translate.pid"),
            "warn",
        )
        .unwrap();

        assert!(unit.contains("WantedBy=multi-user.target"));
        assert!(unit.contains("After=network-online.target"));
        assert!(unit.contains("--log-level warn"));
    }

    #[test]
    fn escapes_systemd_specifiers_and_whitespace() {
        assert_eq!(systemd_escape("100%"), "100%%");
        assert_eq!(systemd_escape("$HOME"), "$$HOME");
        assert_eq!(systemd_escape("two words"), "\"two words\"");
        assert_eq!(systemd_escape("a\"b"), "\"a\\\"b\"");
    }

    #[test]
    fn rejects_newlines_in_arguments() {
        let mut config = test_config();
        config.model = "bad\nmodel".to_owned();
        let error = render_unit(
            Path::new("/usr/local/bin/index-translate"),
            &config,
            Scope::User,
            Path::new("/run/index-translate.pid"),
            "info",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("newlines"));
    }
}
