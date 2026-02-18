# unix-socket-switcher

This is a fork of [ssh-agent-switcher](https://github.com/jmmv/ssh-agent-switcher)
by Julio Merino, generalized to work with any Unix socket (not just SSH agents).

unix-socket-switcher is a daemon that proxies Unix socket connections to a target
socket discovered via glob patterns. This allows long-lived processes such as
terminal multiplexers like `tmux` or `screen` to access sockets that may change
paths across sessions.

A common use case is SSH/GPG agent forwarding: when you reconnect to an SSH server,
the forwarded agent socket path changes, breaking existing sessions.
unix-socket-switcher solves this by exposing a socket at a well-known location
and forwarding to the real socket found via a glob pattern.

## Radicle note

This project uses [Radicle][radicle] as a primary distributed code collaboration
platform. The Github repo is only a read-only mirror.

Head to [the project's Radicle site][project-radicle] for an up to date version
and more information.

[radicle]: https://radicle.xyz
[project-radicle]: https://app.radicle.xyz/nodes/radicle.dpc.pw

## Major changes from the original

Note: The list might become outdated.

- **Blocking-IO**: Refactored not to require async Rust (tokio) for smaller memory usage.
- **Glob-based discovery**: Replaced OpenSSH-specific directory scanning with
  flexible `--target-glob` patterns.
- **General-purpose**: No longer hardcodes SSH agent naming conventions or
  directory structures. Works with any Unix socket.
- **Systemd socket activation**: Supports systemd-managed sockets.

## Installation

unix-socket-switcher is written in Rust. Install with Cargo:

```sh
cargo install unix-socket-switcher
```

Or build from source:

```sh
make install MODE=release PREFIX="${HOME}/.local"
```

## Usage

### SSH agent forwarding example

Extend your login script (typically `~/.login`, `~/.bash_login`, or `~/.zlogin`)
with the following snippet:

```sh
unix-socket-switcher --daemon \
    --socket-path "/tmp/ssh-agent.${USER}" \
    --target-glob "$HOME/.ssh/agent/*" \
    --target-glob "/tmp/ssh-*/agent.*" \
    2>/dev/null || true
export SSH_AUTH_SOCK="/tmp/ssh-agent.${USER}"
```

For `fish`, extend `~/.config/fish/config.fish` with the following:

```sh
unix-socket-switcher --daemon \
    --socket-path "/tmp/ssh-agent.$USER" \
    --target-glob "$HOME/.ssh/agent/*" \
    --target-glob "/tmp/ssh-*/agent.*" \
    &>/dev/null || true
set -gx SSH_AUTH_SOCK "/tmp/ssh-agent.$USER"
```

### General-purpose example

Proxy any Unix socket that matches a glob pattern:

```sh
unix-socket-switcher --daemon \
    --socket-path "/run/user/$(id -u)/my-proxy.sock" \
    --target-glob "/var/run/my-service/*.sock"
```

## Security considerations

unix-socket-switcher is intended to run under your personal unprivileged account
and does not cross any security boundaries. All this daemon does is expose a
new socket that only you can access and forwards all communication to another
socket to which you must already have access.

*Do not run this as root.*

## AI usage disclosure

[I use LLMs when working on my projects.](https://dpc.pw/posts/personal-ai-usage-disclosure/)
