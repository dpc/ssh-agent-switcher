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

//! Utilities to find a target Unix socket matching glob patterns.

use std::path::PathBuf;
use std::time::Duration;

use log::{debug, info, trace};
use tokio::net::UnixStream;

/// How to sort glob results before attempting connections.
#[derive(Clone, Copy, Debug)]
pub enum GlobSort {
    /// Sort alphabetically by path name.
    Name,
    /// Sort by filesystem modification time, oldest first.
    TimestampOldest,
    /// Sort by filesystem modification time, newest first.
    TimestampNewest,
}

/// Collects all paths matching the glob patterns, sorted according to `sort`.
fn collect_paths(globs: &[String], sort: GlobSort) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for pattern in globs {
        let entries = match glob::glob(pattern) {
            Ok(entries) => entries,
            Err(e) => {
                debug!("Invalid glob pattern '{}': {}", pattern, e);
                continue;
            }
        };

        for entry in entries {
            match entry {
                Ok(path) => paths.push(path),
                Err(e) => trace!("Error reading glob entry: {}", e),
            }
        }
    }

    match sort {
        GlobSort::Name => paths.sort(),
        GlobSort::TimestampOldest => {
            paths.sort_by(|a, b| {
                let mtime = |p: &PathBuf| p.metadata().and_then(|m| m.modified()).ok();
                mtime(a).cmp(&mtime(b))
            });
        }
        GlobSort::TimestampNewest => {
            paths.sort_by(|a, b| {
                let mtime = |p: &PathBuf| p.metadata().and_then(|m| m.modified()).ok();
                mtime(b).cmp(&mtime(a))
            });
        }
    }

    paths
}

/// Attempts to connect to the first reachable socket from `paths`.
async fn try_connect(paths: &[PathBuf], timeout: Option<Duration>) -> Option<UnixStream> {
    for path in paths {
        let result = if let Some(timeout) = timeout {
            match tokio::time::timeout(timeout, UnixStream::connect(&path)).await {
                Ok(r) => r,
                Err(_) => {
                    debug!("Connection to {} timed out", path.display());
                    continue;
                }
            }
        } else {
            UnixStream::connect(&path).await
        };

        match result {
            Ok(stream) => {
                info!("Successfully connected to {}", path.display());
                return Some(stream);
            }
            Err(e) => {
                trace!("Cannot connect to {}: {}", path.display(), e);
            }
        }
    }

    None
}

/// Expands the given glob patterns and attempts to connect to the first
/// matching Unix socket.
///
/// Primary `target_globs` are tried first. If none succeeds, `fallback_globs`
/// are tried. Both sets are sorted according to `sort`. If `timeout` is `Some`,
/// each connection attempt is bounded by that duration.
pub(super) async fn find_socket(
    target_globs: &[String],
    fallback_globs: &[String],
    timeout: Option<Duration>,
    sort: GlobSort,
) -> Option<UnixStream> {
    let paths = collect_paths(target_globs, sort);
    if let Some(stream) = try_connect(&paths, timeout).await {
        return Some(stream);
    }

    if !fallback_globs.is_empty() {
        let fallback_paths = collect_paths(fallback_globs, sort);
        if let Some(stream) = try_connect(&fallback_paths, timeout).await {
            return Some(stream);
        }
    }

    None
}
