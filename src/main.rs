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
use std::io;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::time::Duration;

use daemonize::{Daemonize, Outcome};
use getoptsargs::prelude::*;
use listenfd::ListenFd;
use log::info;
use xdg::BaseDirectories;

/// Maximum amount of time to wait for the child process to start when
/// daemonization is enabled.
const MAX_CHILD_WAIT: Duration = Duration::from_secs(10);

/// Gets the value of the `--target-glob` flag.
fn get_target_globs(matches: &Matches) -> Result<Vec<String>> {
    let globs = matches.opt_strs("target-glob");
    if globs.is_empty() {
        bail!("At least one --target-glob must be specified");
    }
    Ok(globs)
}

/// Returns the default value of the `--log-file` flag.
fn default_log_file(xdg_dirs: &BaseDirectories) -> Result<PathBuf> {
    xdg_dirs
        .place_state_file("unix-socket-switcher.log")
        .map_err(|e| anyhow!("Cannot create XDG_STATE_HOME: {}", e))
}

/// Gets the value of the `--log-file` flag, computing a default if necessary.
fn get_log_file(matches: &Matches, xdg_dirs: &BaseDirectories) -> Result<PathBuf> {
    match matches.opt_str("log-file") {
        Some(s) => Ok(PathBuf::from(s)),
        None => default_log_file(xdg_dirs),
    }
}

/// Returns the default value of the `--pid-file` flag.
fn default_pid_file(xdg_dirs: &BaseDirectories) -> Result<PathBuf> {
    match xdg_dirs.place_runtime_file("unix-socket-switcher.pid") {
        Ok(path) => Ok(path),
        Err(_) => {
            // XDG_RUNTIME_DIR *must* be set, but it's quite annoying to fail when it's not.
            // The variable being missing is the default case for FreeBSD, so make this more
            // friendly in that case.
            xdg_dirs
                .place_state_file("unix-socket-switcher.pid")
                .map_err(|e| anyhow!("Cannot create XDG_RUNTIME_DIR: {}", e))
        }
    }
}

/// Gets the value of the `--pid-file` flag, computing a default if necessary.
fn get_pid_file(matches: &Matches, xdg_dirs: &BaseDirectories) -> Result<PathBuf> {
    match matches.opt_str("pid-file") {
        Some(s) => Ok(PathBuf::from(s)),
        None => default_pid_file(xdg_dirs),
    }
}

/// Gets the value of the required `--socket-path` flag.
fn get_socket_path(matches: &Matches) -> Result<PathBuf> {
    match matches.opt_str("socket-path") {
        Some(s) => Ok(PathBuf::from(s)),
        None => bail!("--socket-path must be specified"),
    }
}

fn app_extra_help(output: &mut dyn io::Write) -> io::Result<()> {
    let xdg_dirs = BaseDirectories::new();
    if let Ok(log_file) = default_log_file(&xdg_dirs) {
        writeln!(
            output,
            "If --log-file is not set, the default path is {}",
            log_file.display()
        )?;
    }
    if let Ok(pid_file) = default_pid_file(&xdg_dirs) {
        writeln!(
            output,
            "If --pid-file is not set, the default path is {}",
            pid_file.display()
        )?;
    }

    Ok(())
}

fn app_setup(builder: Builder) -> Builder {
    builder
        .bugs("https://github.com/dpc/unix-socket-switcher/issues/")
        .copyright("Copyright 2023-2026 Julio Merino")
        .homepage("https://github.com/dpc/unix-socket-switcher/")
        .extra_help(app_extra_help)
        .disable_init_env_logger()
        .optmulti(
            "",
            "target-glob",
            "glob pattern for target Unix socket(s) to connect to (can be repeated)",
            "GLOB",
        )
        .optflag("", "daemon", "run in the background")
        .optopt(
            "",
            "log-file",
            "path to the file where to write logs",
            "path",
        )
        .optopt("", "pid-file", "path to the PID file to create", "path")
        .optopt(
            "",
            "socket-path",
            "path to the socket to listen on (required unless using systemd activation)",
            "path",
        )
}

fn daemon_parent(log_file: PathBuf, pid_file: PathBuf) -> Result<i32> {
    info!("Log file: {}", log_file.display());
    info!("PID file: {}", pid_file.display());
    // Socket is already created before daemonizing, so we only wait for the PID
    // file
    let pid_content = unix_socket_switcher::wait_for_file(&pid_file, MAX_CHILD_WAIT)
        .map_err(|e| anyhow!("Daemon failed to start on time: {}", e))?;
    info!("PID is: {}", pid_content.trim());
    Ok(0)
}

fn daemon_child(
    listener: UnixListener,
    target_globs: &[String],
    pid_file: PathBuf,
    systemd_activated: bool,
) -> Result<i32> {
    if let Err(e) = unix_socket_switcher::run(listener, target_globs, pid_file, systemd_activated) {
        bail!("{}", e);
    }
    Ok(0)
}

fn app_main(matches: Matches) -> Result<i32> {
    let xdg_dirs = BaseDirectories::new();

    let target_globs = get_target_globs(&matches)?;
    let log_file = get_log_file(&matches, &xdg_dirs)?;
    let pid_file = get_pid_file(&matches, &xdg_dirs)?;

    // Check for systemd socket activation first
    let mut listenfd = ListenFd::from_env();
    let (listener, systemd_activated) = if let Some(listener) = listenfd.take_unix_listener(0)? {
        if matches.opt_present("socket-path") {
            bail!("Cannot use --socket-path with systemd socket activation");
        }
        info!("Using systemd socket activation");
        (listener, true)
    } else {
        // No systemd socket, create our own
        let socket_path = get_socket_path(&matches)?;
        let listener =
            unix_socket_switcher::create_listener(&socket_path).map_err(|e| anyhow!("{}", e))?;
        (listener, false)
    };

    if matches.opt_present("daemon") {
        if systemd_activated {
            bail!("Cannot use --daemon with systemd socket activation");
        }

        let socket_path = get_socket_path(&matches)?;
        let log = File::options()
            .append(true)
            .create(true)
            .open(&log_file)
            .map_err(|e| {
                anyhow!(
                    "Failed to open/create log file {}: {}",
                    log_file.display(),
                    e
                )
            })?;

        match Daemonize::new().pid_file(&pid_file).stderr(log).execute() {
            Outcome::Parent(Ok(_parent)) => {
                init_env_logger(&matches.program_name);
                daemon_parent(log_file, pid_file)
            }
            Outcome::Parent(Err(e)) => {
                bail!("Failed to become daemon: {}", e);
            }
            Outcome::Child(Ok(_child)) => {
                init_env_logger(&matches.program_name);
                daemon_child(listener, &target_globs, pid_file, systemd_activated)
            }
            Outcome::Child(Err(e)) => {
                let msg = e.to_string();
                if !msg.contains("unable to lock pid file") {
                    // Clean up the socket we created before failing
                    let _ = fs::remove_file(&socket_path);
                    bail!("Failed to become daemon: {}", e);
                }
                // Already running - clean up the socket we created
                let _ = fs::remove_file(&socket_path);
                Ok(0)
            }
        }
    } else {
        init_env_logger(&matches.program_name);
        if !systemd_activated {
            info!("Running in the foreground: ignoring --log-file and --pid-file");
        }
        daemon_child(listener, &target_globs, pid_file, systemd_activated)
    }
}

app!("unix-socket-switcher", app_setup, app_main);
