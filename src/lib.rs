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

use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use std::{fs, io};

use log::{debug, info, warn};
use tokio::net::{UnixListener as TokioUnixListener, UnixStream};
use tokio::signal::unix::{SignalKind, signal};

/// Tracks the number of active proxy connections.
struct ActiveConnections(Arc<AtomicUsize>);

/// RAII guard that decrements the active connection count on drop.
struct ConnectionGuard(Arc<AtomicUsize>);

impl ActiveConnections {
    fn new() -> Self {
        Self(Arc::new(AtomicUsize::new(0)))
    }

    fn guard(&self) -> ConnectionGuard {
        self.0.fetch_add(1, Ordering::Relaxed);
        ConnectionGuard(Arc::clone(&self.0))
    }

    fn count(&self) -> usize {
        self.0.load(Ordering::Relaxed)
    }
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

mod find;

pub use find::GlobSort;

/// Result type for this crate.
type Result<T> = std::result::Result<T, String>;

/// A scope guard to restore the previous umask.
struct UmaskGuard {
    old_umask: libc::mode_t,
}

impl Drop for UmaskGuard {
    fn drop(&mut self) {
        let _ = unsafe { libc::umask(self.old_umask) };
    }
}

/// Sets the umask and returns a guard to restore it on drop.
fn set_umask(umask: libc::mode_t) -> UmaskGuard {
    UmaskGuard {
        old_umask: unsafe { libc::umask(umask) },
    }
}

/// Blocks shutdown signals (SIGINT, SIGQUIT, SIGTERM) so they don't kill the
/// process with the default handler before async signal handlers are
/// registered.
///
/// Must be called before starting the tokio runtime. Signals are unblocked
/// inside [`run`] after the async handlers are set up.
pub fn block_shutdown_signals() {
    unsafe {
        let mut mask: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut mask);
        libc::sigaddset(&mut mask, libc::SIGINT);
        libc::sigaddset(&mut mask, libc::SIGQUIT);
        libc::sigaddset(&mut mask, libc::SIGTERM);
        libc::sigprocmask(libc::SIG_BLOCK, &mask, std::ptr::null_mut());
    }
}

/// Creates the agent socket to listen on.
///
/// This makes sure that the socket is only accessible by the current user.
pub fn create_listener(socket_path: &Path) -> Result<UnixListener> {
    // Ensure the socket is not group nor world readable so that we don't expose the
    // real socket indirectly to other users.
    let _guard = set_umask(0o177);

    UnixListener::bind(socket_path)
        .map_err(|e| format!("Cannot listen on {}: {}", socket_path.display(), e))
}

/// Handles one incoming connection on `client`.
async fn handle_connection(
    mut client: UnixStream,
    target_globs: &[String],
    fallback_globs: &[String],
    connect_timeout: Option<Duration>,
    glob_sort: GlobSort,
) -> Result<()> {
    let mut agent =
        match find::find_socket(target_globs, fallback_globs, connect_timeout, glob_sort).await {
            Some(socket) => socket,
            None => {
                return Err("No target socket found; cannot proxy request".to_owned());
            }
        };
    let result = tokio::io::copy_bidirectional(&mut client, &mut agent)
        .await
        .map(|_| ())
        .map_err(|e| format!("{}", e));
    debug!("Closing client connection");
    result
}

/// Runs the core logic of the app.
///
/// This serves the listening socket using the provided `listener` and looks for
/// target sockets matching `target_globs`.
///
/// If `pid_file` is provided, it will be cleaned up on exit. If
/// `systemd_activated` is true, the socket file will not be removed on exit
/// (systemd owns it). If `idle_timeout` is provided, the process exits after
/// being idle (no active connections) for that duration.
pub async fn run(
    listener: UnixListener,
    target_globs: &[String],
    fallback_globs: &[String],
    pid_file: Option<PathBuf>,
    systemd_activated: bool,
    idle_timeout: Option<Duration>,
    connect_timeout: Option<Duration>,
    glob_sort: GlobSort,
) -> Result<()> {
    let socket_path = listener
        .local_addr()
        .ok()
        .and_then(|addr| addr.as_pathname().map(|p| p.to_path_buf()))
        .ok_or_else(|| "Cannot determine socket path from listener".to_string())?;

    listener
        .set_nonblocking(true)
        .map_err(|e| format!("Failed to set listener non-blocking: {}", e))?;
    let listener = TokioUnixListener::from_std(listener)
        .map_err(|e| format!("Failed to create tokio listener: {}", e))?;

    let mut sighup = signal(SignalKind::hangup())
        .map_err(|e| format!("Failed to install SIGHUP handler: {}", e))?;
    let mut sigint = signal(SignalKind::interrupt())
        .map_err(|e| format!("Failed to install SIGINT handler: {}", e))?;
    let mut sigquit = signal(SignalKind::quit())
        .map_err(|e| format!("Failed to install SIGQUIT handler: {}", e))?;
    let mut sigterm = signal(SignalKind::terminate())
        .map_err(|e| format!("Failed to install SIGTERM handler: {}", e))?;

    // Unblock signals now that tokio handlers are registered.
    // Any pending signals are delivered to the handlers immediately.
    unsafe {
        let mut mask: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut mask);
        libc::sigaddset(&mut mask, libc::SIGINT);
        libc::sigaddset(&mut mask, libc::SIGQUIT);
        libc::sigaddset(&mut mask, libc::SIGTERM);
        libc::sigprocmask(libc::SIG_UNBLOCK, &mask, std::ptr::null_mut());
    }

    let target_globs: Arc<[String]> = target_globs.into();
    let fallback_globs: Arc<[String]> = fallback_globs.into();
    let active_connections = ActiveConnections::new();

    let idle_sleep = tokio::time::sleep(idle_timeout.unwrap_or(Duration::MAX));
    tokio::pin!(idle_sleep);

    debug!("Entering main loop");
    let mut stop = None;
    while stop.is_none() {
        tokio::select! {
            result = listener.accept() => match result {
                Ok((socket, _addr)) => {
                    debug!("Connection accepted");
                    let guard = active_connections.guard();
                    let globs = Arc::clone(&target_globs);
                    let fb_globs = Arc::clone(&fallback_globs);
                    tokio::spawn(async move {
                        let _guard = guard;
                        if let Err(e) = handle_connection(socket, &globs, &fb_globs, connect_timeout, glob_sort).await {
                            warn!("Dropping connection due to error: {}", e);
                        }
                    });
                    if let Some(timeout) = idle_timeout {
                        idle_sleep.as_mut().reset(tokio::time::Instant::now() + timeout);
                    }
                }
                Err(e) => warn!("Failed to accept connection: {}", e),
            },

            () = &mut idle_sleep, if idle_timeout.is_some() => {
                if active_connections.count() == 0 {
                    stop = Some("idle timeout");
                } else {
                    debug!("Idle timer fired but {} connections still active", active_connections.count());
                    idle_sleep.as_mut().reset(tokio::time::Instant::now() + idle_timeout.unwrap());
                }
            },

            _ = sighup.recv() => (),
            _ = sigint.recv() => stop = Some("SIGINT"),
            _ = sigquit.recv() => stop = Some("SIGQUIT"),
            _ = sigterm.recv() => stop = Some("SIGTERM"),
        }
    }
    debug!("Main loop exited");

    let stop = stop.expect("Loop can only exit by setting stop");
    if systemd_activated {
        info!(
            "Shutting down due to {} (systemd owns {})",
            stop,
            socket_path.display()
        );
    } else {
        info!(
            "Shutting down due to {} and removing {}",
            stop,
            socket_path.display()
        );
        let _ = fs::remove_file(&socket_path);
    }

    // Because we catch signals, daemonize doesn't properly clean up the PID file so
    // we have to do it ourselves.
    if let Some(ref pid_file) = pid_file {
        let _ = fs::remove_file(pid_file);
    }

    Ok(())
}

/// Waits for `path` to contain non-empty content for a maximum period of time.
/// Uses `op` to read the file and returns its result on success.
/// Retries on `NotFound` errors and on empty file content.
pub fn wait_for_file(path: &Path, mut pending_wait: Duration) -> Result<String> {
    while pending_wait > Duration::ZERO {
        match fs::read_to_string(path) {
            Ok(content) if content.trim().is_empty() => {
                // File exists but is empty (e.g. PID file not yet written)
                pending_wait -= Duration::from_millis(1);
            }
            Ok(content) => {
                return Ok(content);
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                pending_wait -= Duration::from_millis(1);
            }
            Err(e) => {
                return Err(e.to_string());
            }
        }
    }
    Err("File was not created on time".to_owned())
}
