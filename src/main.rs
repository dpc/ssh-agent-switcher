// Copyright 2025 Julio Merino.
// All rights reserved.
//
// Redistribution and use in source and binary forms, with or without
// modification, are permitted provided that the following conditions are met:
//
// * Redistributions of source code must retain the above copyright notice, this
//   list of conditions and the following disclaimer.
// * Redistributions in binary form must reproduce the above copyright notice,
//   this list of conditions and the following disclaimer in the documentation
//   and/or other materials provided with the distribution.
// * Neither the name of unix-socket-switcher nor the names of its contributors
//   may be used to endorse or promote products derived from this software
//   without specific prior written permission.
//
// THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
// AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
// IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE
// ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT OWNER OR CONTRIBUTORS BE
// LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR
// CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF
// SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS
// INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN
// CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE)
// ARISING IN ANY WAY OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE
// POSSIBILITY OF SUCH DAMAGE.

//! Serves a Unix domain socket that proxies connections to a target Unix socket
//! found via glob patterns.

use std::fs::{self, File};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use clap::{ArgAction, Parser};
use daemonize::{Daemonize, Outcome};
use listenfd::ListenFd;
use log::{debug, info};

/// Maximum amount of time to wait for the child process to start when
/// daemonization is enabled.
const MAX_CHILD_WAIT: Duration = Duration::from_secs(10);

/// Command-line arguments for unix-socket-switcher.
#[derive(Debug, Parser)]
#[command(
    name = "unix-socket-switcher",
    disable_version_flag = true,
    about = "Proxy Unix socket connections to a target socket discovered via glob patterns",
    long_about = None
)]
struct Cli {
    /// Glob pattern for target Unix socket(s) to connect to.
    #[arg(long = "target-glob", value_name = "GLOB")]
    target_globs: Vec<String>,

    /// Fallback glob pattern, tried after all --target-glob entries fail.
    #[arg(long = "target-fallback-glob", value_name = "GLOB")]
    fallback_globs: Vec<String>,

    /// Run in the background.
    #[arg(long)]
    daemon: bool,

    /// Path to the file where to write logs.
    #[arg(long = "log-file", value_name = "path")]
    log_file: Option<PathBuf>,

    /// Path to the PID file to create.
    #[arg(long = "pid-file", value_name = "path")]
    pid_file: Option<PathBuf>,

    /// Path to the socket to listen on.
    #[arg(long = "socket-path", value_name = "path")]
    socket_path: Option<PathBuf>,

    /// Exit after being idle for this many seconds.
    #[arg(long = "idle-timeout", value_name = "SECONDS")]
    idle_timeout: Option<String>,

    /// Timeout in milliseconds for each target socket connection attempt.
    #[arg(long = "connect-timeout", value_name = "MS")]
    connect_timeout: Option<String>,

    /// Sort order for glob results: name, timestamp-oldest, timestamp-newest.
    #[arg(long = "target-glob-sort", value_name = "ORDER")]
    target_glob_sort: Option<String>,

    /// Show version information on stdout and exit.
    #[arg(long = "version", action = ArgAction::SetTrue)]
    show_version: bool,
}

/// Runtime configuration passed to the foreground or daemonized server child.
struct SwitcherConfig {
    /// Glob patterns for primary target Unix socket discovery.
    target_globs: Vec<String>,

    /// Glob patterns for fallback target Unix socket discovery.
    fallback_globs: Vec<String>,

    /// Optional PID file to remove on shutdown.
    pid_file: Option<PathBuf>,

    /// Whether the listener came from systemd socket activation.
    systemd_activated: bool,

    /// Optional idle shutdown timeout.
    idle_timeout: Option<Duration>,

    /// Optional per-target connection timeout.
    connect_timeout: Option<Duration>,

    /// Sort order for glob matches.
    glob_sort: unix_socket_switcher::GlobSort,
}

/// Gets the value of the `--target-glob` flag.
fn get_target_globs(cli: &Cli) -> Result<&[String]> {
    if cli.target_globs.is_empty() {
        bail!("At least one --target-glob must be specified");
    }
    Ok(&cli.target_globs)
}

/// Gets the value of the required `--socket-path` flag.
fn get_socket_path(cli: &Cli) -> Result<&Path> {
    match cli.socket_path.as_deref() {
        Some(path) => Ok(path),
        None => bail!("--socket-path must be specified"),
    }
}

/// Gets the value of the `--idle-timeout` flag, if specified.
fn get_idle_timeout(cli: &Cli) -> Result<Option<Duration>> {
    match &cli.idle_timeout {
        Some(s) => {
            let secs: u64 = s
                .parse()
                .map_err(|_| anyhow!("--idle-timeout must be a positive integer (seconds)"))?;
            if secs == 0 {
                bail!("--idle-timeout must be a positive integer (seconds)");
            }
            Ok(Some(Duration::from_secs(secs)))
        }
        None => Ok(None),
    }
}

/// Gets the value of the `--connect-timeout` flag, if specified.
fn get_connect_timeout(cli: &Cli) -> Result<Option<Duration>> {
    match &cli.connect_timeout {
        Some(s) => {
            let ms: u64 = s.parse().map_err(|_| {
                anyhow!("--connect-timeout must be a positive integer (milliseconds)")
            })?;
            if ms == 0 {
                bail!("--connect-timeout must be a positive integer (milliseconds)");
            }
            Ok(Some(Duration::from_millis(ms)))
        }
        None => Ok(None),
    }
}

/// Initializes env_logger with the same default formatting as the old CLI glue.
fn init_env_logger(program_name: impl Into<String>) {
    use std::io::Write;

    let mut builder =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));
    {
        let program_name = program_name.into();
        builder.format(move |buf, record| {
            writeln!(
                buf,
                "{}: {}: {}",
                program_name,
                record.level(),
                record.args()
            )
        });
    }
    builder.init();
}

fn daemon_parent(log_file: Option<&Path>, pid_file: Option<&Path>) -> Result<i32> {
    if let Some(log_file) = log_file {
        info!("Log file: {}", log_file.display());
    }
    if let Some(pid_file) = pid_file {
        info!("PID file: {}", pid_file.display());
        let pid_content = unix_socket_switcher::wait_for_file(pid_file, MAX_CHILD_WAIT)
            .map_err(|e| anyhow!("Daemon failed to start on time: {}", e))?;
        info!("PID is: {}", pid_content.trim());
    }
    Ok(0)
}

/// Prints version information in the historical getoptsargs format.
fn print_version() {
    println!("unix-socket-switcher {}", env!("CARGO_PKG_VERSION"));
    println!("Copyright 2023-2026 Julio Merino");
    println!("License {}", env!("CARGO_PKG_LICENSE"));
}

fn daemon_child(listener: UnixListener, config: SwitcherConfig) -> Result<i32> {
    // Block shutdown signals before creating the runtime so an early SIGTERM
    // doesn't kill the process.  They are unblocked inside run() after async
    // signal handlers are registered.
    unix_socket_switcher::block_shutdown_signals();

    let runtime =
        tokio::runtime::Runtime::new().map_err(|e| anyhow!("Failed to start runtime: {}", e))?;
    runtime.block_on(async {
        if let Err(e) = unix_socket_switcher::run(
            listener,
            &config.target_globs,
            &config.fallback_globs,
            config.pid_file,
            config.systemd_activated,
            config.idle_timeout,
            config.connect_timeout,
            config.glob_sort,
        )
        .await
        {
            bail!("{}", e);
        }
        Ok(0)
    })
}

fn get_glob_sort(cli: &Cli) -> Result<unix_socket_switcher::GlobSort> {
    use unix_socket_switcher::GlobSort;

    match cli.target_glob_sort.as_deref() {
        None | Some("name") => Ok(GlobSort::Name),
        Some("timestamp-oldest") => Ok(GlobSort::TimestampOldest),
        Some("timestamp-newest") => Ok(GlobSort::TimestampNewest),
        Some(other) => bail!(
            "Invalid --target-glob-sort value '{}'; expected name, timestamp-oldest, or timestamp-newest",
            other
        ),
    }
}

fn app_main(cli: Cli, program_name: &str) -> Result<i32> {
    if cli.show_version {
        print_version();
        return Ok(0);
    }

    let target_globs = get_target_globs(&cli)?.to_vec();
    let fallback_globs = cli.fallback_globs.clone();
    let log_file = cli.log_file.clone();
    let pid_file = cli.pid_file.clone();
    let idle_timeout = get_idle_timeout(&cli)?;
    let connect_timeout = get_connect_timeout(&cli)?;
    let glob_sort = get_glob_sort(&cli)?;

    // Save socket activation env vars for diagnostics (ListenFd::from_env() clears
    // them).
    let listen_fds_env = std::env::var("LISTEN_FDS").ok();
    let listen_pid_env = std::env::var("LISTEN_PID").ok();

    // Check for systemd socket activation first, fall back to --socket-path.
    let mut listenfd = ListenFd::from_env();
    let (listener, systemd_activated) = if let Some(listener) = listenfd.take_unix_listener(0)? {
        if cli.socket_path.is_some() {
            bail!("Cannot use --socket-path with systemd socket activation");
        }
        info!("Using systemd socket activation");
        (listener, true)
    } else {
        // No systemd socket, create our own
        let socket_path = get_socket_path(&cli)?;
        // Remove any leftover socket file from a previous instance so bind() succeeds.
        if let Err(e) = fs::remove_file(socket_path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            bail!(
                "Failed to remove stale socket {}: {}",
                socket_path.display(),
                e
            );
        }
        let listener =
            unix_socket_switcher::create_listener(socket_path).map_err(|e| anyhow!("{}", e))?;
        (listener, false)
    };

    if cli.daemon {
        if systemd_activated {
            bail!("Cannot use --daemon with systemd socket activation");
        }

        let socket_path = get_socket_path(&cli)?;

        let mut daemonize = Daemonize::new();
        if let Some(ref pid_file) = pid_file {
            daemonize = daemonize.pid_file(pid_file);
        }
        if let Some(ref log_file) = log_file {
            let log = File::options()
                .append(true)
                .create(true)
                .open(log_file)
                .map_err(|e| {
                    anyhow!(
                        "Failed to open/create log file {}: {}",
                        log_file.display(),
                        e
                    )
                })?;
            daemonize = daemonize.stderr(log);
        }

        match daemonize.execute() {
            Outcome::Parent(Ok(_parent)) => {
                init_env_logger(program_name);
                daemon_parent(log_file.as_deref(), pid_file.as_deref())
            }
            Outcome::Parent(Err(e)) => {
                bail!("Failed to become daemon: {}", e);
            }
            Outcome::Child(Ok(_child)) => {
                init_env_logger(program_name);
                daemon_child(
                    listener,
                    SwitcherConfig {
                        target_globs,
                        fallback_globs,
                        pid_file,
                        systemd_activated,
                        idle_timeout,
                        connect_timeout,
                        glob_sort,
                    },
                )
            }
            Outcome::Child(Err(e)) => {
                let msg = e.to_string();
                if !msg.contains("unable to lock pid file") {
                    // Clean up the socket we created before failing
                    let _ = fs::remove_file(socket_path);
                    bail!("Failed to become daemon: {}", e);
                }
                // Already running - clean up the socket we created
                let _ = fs::remove_file(socket_path);
                Ok(0)
            }
        }
    } else {
        init_env_logger(program_name);
        debug!(
            "Socket activation env: LISTEN_FDS={:?}, LISTEN_PID={:?}, pid={}",
            listen_fds_env,
            listen_pid_env,
            std::process::id()
        );
        daemon_child(
            listener,
            SwitcherConfig {
                target_globs,
                fallback_globs,
                pid_file,
                systemd_activated,
                idle_timeout,
                connect_timeout,
                glob_sort,
            },
        )
    }
}

fn main() {
    let program_name = std::env::args()
        .next()
        .and_then(|arg| {
            PathBuf::from(arg)
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
        })
        .unwrap_or_else(|| "unix-socket-switcher".to_string());
    let cli = Cli::parse();

    match app_main(cli, &program_name) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("{}: {}", program_name, e);
            std::process::exit(1);
        }
    }
}
