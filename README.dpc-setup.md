# dpc's SSH/GPG socket forwarding setup

I use `unix-socket-switcher` to keep stable SSH and GPG socket paths while the
actual forwarded-agent sockets change with each SSH connection.

## Fixed local sockets

On each host, systemd user socket activation owns two stable socket paths and
starts `unix-socket-switcher` on demand:

- `$XDG_RUNTIME_DIR/S.ssh-agent.mux` for SSH agent clients.
- `$XDG_RUNTIME_DIR/gnupg/S.gpg-agent` for GPG clients.

The shell exports the fixed SSH path as `SSH_AUTH_SOCK`, so tmux and long-lived
processes never need environment updates. GPG already uses a standard path; the
real local GPG agent is moved aside to
`$XDG_RUNTIME_DIR/gnupg/S.gpg-agent.local` so the switcher can own the standard
socket.

## SSH agent selection

The SSH switcher selects the newest forwarded SSH agent socket, with the local
GPG agent's SSH socket as fallback:

```sh
unix-socket-switcher \
    --target-glob-sort=timestamp-newest \
    --target-glob "$HOME/.ssh/agent/*" \
    --target-fallback-glob "$XDG_RUNTIME_DIR/gnupg/S.gpg-agent.ssh"
```

When SSH agent forwarding creates a fresh remote agent socket, clients using the
fixed mux socket automatically reach that newest forwarded agent. If there is no
forwarded agent, they use the local agent instead.

## GPG agent selection

SSH is configured to remote-forward the local GPG extra socket to a per-source
socket on the remote host:

```sshconfig
RemoteForward /run/user/<uid>/gnupg/S.gpg-agent-remote.%L \
              /run/user/<uid>/gnupg/S.gpg-agent.extra
```

On the remote side, the GPG switcher listens on the standard
`S.gpg-agent` path and chooses the newest forwarded GPG socket, falling back to
the local GPG agent:

```sh
unix-socket-switcher \
    --target-glob-sort=timestamp-newest \
    --target-glob "$XDG_RUNTIME_DIR/gnupg/S.gpg-agent-remote.*" \
    --target-fallback-glob "$XDG_RUNTIME_DIR/gnupg/S.gpg-agent.local"
```

Using `%L` gives each source host a distinct remote-forwarded socket name, so
multiple inbound SSH sessions can coexist. Picking the newest socket makes the
most recent connection win, which is the one most likely to point back to the
machine currently in use.

## Between hosts

Every host is configured the same way: clients talk to stable local socket paths,
while SSH creates new forwarded sockets when a connection arrives. The switchers
hide those changing paths by selecting the newest forwarded socket and falling
back to the local agent when no forward is present.

This makes agent use work across nested SSH sessions without updating tmux
environments or application configuration.
