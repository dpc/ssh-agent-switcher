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

use daemonize::{Daemonize, Outcome};
use getoptsargs::prelude::*;
use listenfd::ListenFd;
use log::info;

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

/// Gets the value of the `--log-file` flag, if specified.
fn get_log_file(matches: &Matches) -> Option<PathBuf> {
    matches.opt_str("log-file").map(PathBuf::from)
}

/// Gets the value of the `--pid-file` flag, if specified.
fn get_pid_file(matches: &Matches) -> Option<PathBuf> {
    matches.opt_str("pid-file").map(PathBuf::from)
}

/// Gets the value of the required `--socket-path` flag.
fn get_socket_path(matches: &Matches) -> Result<PathBuf> {
    match matches.opt_str("socket-path") {
        Some(s) => Ok(PathBuf::from(s)),
        None => bail!("--socket-path must be specified"),
    }
}

fn app_setup(builder: Builder) -> Builder {
    builder
        .bugs("https://github.com/dpc/unix-socket-switcher/issues/")
        .copyright("Copyright 2023-2026 Julio Merino")
        .homepage("https://github.com/dpc/unix-socket-switcher/")
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

fn daemon_child(
    listener: UnixListener,
    target_globs: &[String],
    pid_file: Option<PathBuf>,
    systemd_activated: bool,
) -> Result<i32> {
    if let Err(e) = unix_socket_switcher::run(listener, target_globs, pid_file, systemd_activated) {
        bail!("{}", e);
    }
    Ok(0)
}

fn app_main(matches: Matches) -> Result<i32> {
    let target_globs = get_target_globs(&matches)?;
    let log_file = get_log_file(&matches);
    let pid_file = get_pid_file(&matches);

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
                init_env_logger(&matches.program_name);
                daemon_parent(log_file.as_deref(), pid_file.as_deref())
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
        daemon_child(listener, &target_globs, pid_file, systemd_activated)
    }
}

app!("unix-socket-switcher", app_setup, app_main);
