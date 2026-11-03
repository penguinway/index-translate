use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use index_translate::{
    config::{Config, DEFAULT_API_BASE, DEFAULT_MODEL, DEFAULT_PORT},
    daemon::{self, health_probe, DAEMON_CHILD_ENV},
    proxy::{build_router, AppState},
    service::{self, InstallOptions, Scope},
    upstream::build_client,
};
use std::{
    env,
    fs::OpenOptions,
    io::IsTerminal,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command as ProcessCommand, Stdio},
    thread::sleep,
    time::{Duration, Instant},
};
use tokio::net::TcpListener;
use tracing::{info, warn};

const SHUTDOWN_GRACE: Duration = Duration::from_secs(8);

#[derive(Parser, Debug)]
#[command(
    name = "index-translate",
    version,
    about = "Local OpenAI-compatible bridge between Immersive Translate and the free Index-Translate API",
    long_about = None
)]
struct Cli {
    #[command(flatten)]
    common: Common,

    #[command(subcommand)]
    action: Option<Action>,
}

#[derive(Args, Debug, Clone)]
struct Common {
    /// Upstream Index-Translate API base URL
    #[arg(
        long,
        global = true,
        env = "INDEX_TRANSLATE_API_BASE",
        default_value = DEFAULT_API_BASE
    )]
    api_base: String,

    /// Interface to bind (use 0.0.0.0 to expose the proxy on the LAN)
    #[arg(long, short = 'H', global = true, default_value = "127.0.0.1")]
    host: String,

    /// Local TCP port
    #[arg(long, short = 'p', global = true, default_value_t = DEFAULT_PORT)]
    port: u16,

    /// Default model advertised by /v1/models
    #[arg(
        long,
        short = 'm',
        global = true,
        default_value = DEFAULT_MODEL
    )]
    model: String,

    /// PID file (default: $XDG_RUNTIME_DIR/index-translate.pid)
    #[arg(long, global = true, value_name = "PATH")]
    pid_file: Option<PathBuf>,

    /// Log file used in background mode (default: $XDG_STATE_HOME/index-translate/index-translate.log)
    #[arg(long, global = true, value_name = "PATH")]
    log_file: Option<PathBuf>,

    /// Log level or EnvFilter directive
    #[arg(
        long,
        global = true,
        env = "INDEX_TRANSLATE_LOG",
        default_value = "info"
    )]
    log_level: String,

    /// Log every proxied request
    #[arg(long, global = true)]
    access_log: bool,

    /// Maximum accepted request body size in MiB
    #[arg(
        long,
        global = true,
        default_value_t = 64,
        value_parser = clap::value_parser!(u64).range(1..=1024)
    )]
    max_body_mib: u64,
}

#[derive(Subcommand, Debug, Clone, Copy)]
enum Action {
    /// Run in the foreground (default)
    Serve,
    /// Start in the background and detach from the terminal
    Start,
    /// Stop the background instance
    Stop,
    /// Restart the background instance
    Restart,
    /// Show background instance status
    Status,
    /// Install and start a systemd user service (alias: install)
    #[command(alias = "install")]
    InstallService {
        /// Install a system-wide unit under /etc/systemd/system (requires root)
        #[arg(long)]
        system: bool,
        /// Start the user service at boot even before login (loginctl enable-linger)
        #[arg(long)]
        linger: bool,
    },
    /// Stop, disable, and remove the systemd service (alias: uninstall)
    #[command(alias = "uninstall")]
    UninstallService {
        /// Remove the system-wide unit instead of the user unit (requires root)
        #[arg(long)]
        system: bool,
    },
}

impl Common {
    fn to_config(&self) -> Result<Config> {
        let max_body_bytes = usize::try_from(self.max_body_mib)
            .ok()
            .and_then(|mib| mib.checked_mul(1024 * 1024))
            .context("--max-body-mib is too large for this platform")?;
        Config::new(
            &self.api_base,
            &self.model,
            &self.host,
            self.port,
            self.access_log,
            max_body_bytes,
        )
    }

    fn pid_path(&self) -> PathBuf {
        self.pid_file
            .clone()
            .unwrap_or_else(daemon::default_pid_file)
    }

    fn log_path(&self) -> PathBuf {
        self.log_file
            .clone()
            .unwrap_or_else(daemon::default_log_file)
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("index-translate: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let config = cli.common.to_config()?;
    let pid_path = cli.common.pid_path();
    let log_path = cli.common.log_path();

    // The parent re-executes this binary with this variable set, so the child
    // serves in the foreground while being fully detached from the terminal.
    let is_daemon_child = env::var_os(DAEMON_CHILD_ENV).is_some();
    let action = if is_daemon_child {
        Action::Serve
    } else {
        cli.action.unwrap_or(Action::Serve)
    };

    match action {
        Action::Serve => serve(&cli.common, config, &pid_path),
        Action::Start => start(&config, &pid_path, &log_path),
        Action::Stop => stop(&pid_path),
        Action::Restart => {
            stop(&pid_path)?;
            sleep(Duration::from_millis(300));
            start(&config, &pid_path, &log_path)
        }
        Action::Status => status(&config, &pid_path, &log_path),
        Action::InstallService { system, linger } => service::install(&InstallOptions {
            scope: if system { Scope::System } else { Scope::User },
            config: &config,
            pid_file: cli.common.pid_file.as_deref(),
            log_level: &cli.common.log_level,
            linger,
        }),
        Action::UninstallService { system } => {
            service::uninstall(if system { Scope::System } else { Scope::User })
        }
    }
}

fn serve(common: &Common, config: Config, pid_path: &Path) -> Result<()> {
    let guard = daemon::PidGuard::acquire(pid_path)?;
    init_logging(&common.log_level)?;

    info!(
        pid = guard.pid(),
        pid_file = %pid_path.display(),
        "index-translate starting"
    );

    if !config.is_loopback() {
        warn!(
            host = %config.host,
            "binding to a non-loopback interface exposes this unauthenticated proxy to other hosts"
        );
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to create the async runtime")?;
    runtime.block_on(serve_async(config))
}

async fn serve_async(config: Config) -> Result<()> {
    let client = build_client()?;
    let bind_addr = config.bind_addr()?;
    let listener = TcpListener::bind(bind_addr)
        .await
        .with_context(|| format!("failed to bind {bind_addr}"))?;
    let local_addr = listener
        .local_addr()
        .context("failed to read local address")?;

    print_banner(&config, local_addr.port());

    let app = build_router(AppState::new(config, client));
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    tokio::spawn(async move {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler");
        let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
            .expect("failed to install SIGINT handler");
        let signal = tokio::select! {
            _ = sigterm.recv() => "SIGTERM",
            _ = sigint.recv() => "SIGINT",
        };
        info!("received {signal}, shutting down");
        let _ = shutdown_tx.send(true);
    });

    let mut graceful_rx = shutdown_rx.clone();
    let graceful = async move {
        while !*graceful_rx.borrow() {
            if graceful_rx.changed().await.is_err() {
                break;
            }
        }
    };

    let mut force_rx = shutdown_rx;
    let force = async move {
        while !*force_rx.borrow() {
            if force_rx.changed().await.is_err() {
                return;
            }
        }
        tokio::time::sleep(SHUTDOWN_GRACE).await;
        warn!(
            "graceful shutdown timed out after {}s; closing open connections",
            SHUTDOWN_GRACE.as_secs()
        );
    };

    let server = axum::serve(listener, app).with_graceful_shutdown(graceful);
    tokio::select! {
        result = server => result.context("proxy server failed")?,
        _ = force => {}
    }

    Ok(())
}

fn start(config: &Config, pid_path: &Path, log_path: &Path) -> Result<()> {
    if let Some(pid) = daemon::read_pid(pid_path) {
        if daemon::is_our_process(pid) {
            bail!("already running (pid {pid}); use `index-translate restart` or `stop`");
        }
        let _ = std::fs::remove_file(pid_path);
    }

    if let Some(parent) = log_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create log directory {}", parent.display()))?;
    }
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .with_context(|| format!("failed to open log file {}", log_path.display()))?;

    let exe = env::current_exe().context("failed to locate the current executable")?;
    let mut command = ProcessCommand::new(&exe);
    command.args(env::args_os().skip(1));
    command.env(DAEMON_CHILD_ENV, "1");
    command.stdin(Stdio::null());
    command.stdout(Stdio::from(log.try_clone()?));
    command.stderr(Stdio::from(log));

    // `setsid` detaches from the controlling terminal and session, so the child
    // survives the parent shell. Safe to call between fork and exec.
    unsafe {
        command.pre_exec(|| {
            nix::unistd::setsid()
                .map(|_| ())
                .map_err(|error| std::io::Error::from_raw_os_error(error as i32))
        });
    }

    let mut child = command
        .spawn()
        .context("failed to spawn the background process")?;
    let deadline = Instant::now() + Duration::from_secs(10);

    loop {
        if let Some(status) = child
            .try_wait()
            .context("failed to check the background process")?
        {
            if daemon::read_pid(pid_path) == Some(child.id()) {
                let _ = std::fs::remove_file(pid_path);
            }
            if let Some(pid) = daemon::read_pid(pid_path).filter(|pid| daemon::is_our_process(*pid))
            {
                bail!("another instance is already running (pid {pid})");
            }
            bail!(
                "background process exited during startup ({status}); see log: {}",
                log_path.display()
            );
        }

        if daemon::read_pid(pid_path) == Some(child.id()) && health_probe(config) {
            println!("Index-Translate 代理已在后台启动 (pid {})", child.id());
            println!("  接口地址 (API URL): {}", config.endpoint());
            println!("  上游 API (Upstream): {}", config.api_base);
            println!("  默认模型 (Model): {}", config.model);
            println!("  日志 (Log): {}", log_path.display());
            println!("  停止命令: index-translate stop");
            println!();
            println!(
                "沉浸式翻译配置: 服务=自定义(OpenAI 兼容), API URL={}, Model={}, API Key=任意",
                config.endpoint(),
                config.model
            );
            return Ok(());
        }

        if Instant::now() >= deadline {
            let child_pid = child.id();
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(child_pid as i32),
                nix::sys::signal::Signal::SIGTERM,
            );
            for _ in 0..20 {
                if !daemon::process_alive(child_pid) {
                    break;
                }
                sleep(Duration::from_millis(50));
            }
            if daemon::read_pid(pid_path) == Some(child_pid) {
                let _ = std::fs::remove_file(pid_path);
            }
            bail!(
                "background process did not become ready within 10s; see log: {}",
                log_path.display()
            );
        }

        sleep(Duration::from_millis(100));
    }
}

fn stop(pid_path: &Path) -> Result<()> {
    match daemon::stop(pid_path, Duration::from_secs(10))? {
        Some(pid) => println!("Index-Translate 代理已停止 (pid {pid})"),
        None => println!("Index-Translate 代理未在运行"),
    }
    Ok(())
}

fn status(config: &Config, pid_path: &Path, log_path: &Path) -> Result<()> {
    match daemon::read_pid(pid_path) {
        Some(pid) if daemon::is_our_process(pid) => {
            println!("Index-Translate 代理正在运行 (pid {pid})");
            println!("  接口地址 (API URL): {}", config.endpoint());
            println!("  上游 API (Upstream): {}", config.api_base);
            println!("  默认模型 (Model): {}", config.model);
            println!("  日志 (Log): {}", log_path.display());
            println!("  PID 文件: {}", pid_path.display());
        }
        Some(pid) => {
            let _ = std::fs::remove_file(pid_path);
            println!("Index-Translate 代理未在运行 (已清理失效 PID {pid})");
        }
        None => println!("Index-Translate 代理未在运行"),
    }
    Ok(())
}

fn init_logging(level: &str) -> Result<()> {
    let filter = tracing_subscriber::EnvFilter::try_new(level)
        .with_context(|| format!("invalid log level or filter: {level:?}"))?;
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_ansi(std::io::stdout().is_terminal())
        .with_writer(std::io::stdout)
        .init();
    Ok(())
}

fn print_banner(config: &Config, port: u16) {
    let endpoint = format!("http://{}:{}/v1", config.display_host(), port);
    println!("============================================================");
    println!("Index-Translate 本地代理已启动");
    println!("  接口地址 (API URL): {endpoint}");
    println!("  上游 API (Upstream): {}", config.api_base);
    println!("  默认模型 (Model): {}", config.model);
    println!("  PID: {}", std::process::id());
    println!();
    println!("沉浸式翻译配置:");
    println!("  翻译服务: 自定义 (OpenAI 兼容)");
    println!("  API URL: {endpoint}");
    println!("  模型: {}", config.model);
    println!("  API Key: 任意 (如 index)");
    println!("============================================================");
}
