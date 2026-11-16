# agentvm

Run [Claude Code](https://claude.com/claude-code) inside a throwaway
[cloud-hypervisor](https://www.cloudhypervisor.org/) microVM that can only see
the directory you launched it from.

```sh
cd ~/some/project
agentvm                # starts a VM, runs `claude` in it, tears it down on exit
agentvm --resume       # any arguments are passed through to `claude`
```

The guest is a minimal NixOS system built with
[microvm.nix](https://github.com/microvm-nix/microvm.nix). Claude Code runs
directly on the VM's console, which is wired to your terminal. There is no
ssh, no login, and no network interface in the guest.

## What the guest can and cannot reach

| Resource | Access |
|---|---|
| Current directory (the project) | Read/write, mounted at `/work` via virtiofs |
| Rest of the host filesystem (`~`, `~/.ssh`, cloud creds, …) | None |
| Network | Outbound HTTP/HTTPS only, through a proxy built into `agentvm` |
| Host processes / sockets | None, apart from the two vsock channels below |
| Host environment variables | None, unless explicitly listed in `forwardEnv` |

The guest root filesystem read-only, everything that persists lives in the shared directory.

## Requirements

- Linux x86_64 with KVM (`/dev/kvm`) accessible for current user
- Nix with flakes enabled

## Usage

### From this repository

```sh
make run                                  # share this repo's directory
make run ARGS="--resume"                  # pick a session to resume
make run ARGS="--continue"                # continue the most recent session
AGENTVM_FORWARD_ENV=ANTHROPIC_API_KEY ANTHROPIC_API_KEY=sk-... make run   # forward an API key
AGENTVM_DEBUG=1 make run                    # print the guest boot log afterwards
```

`make run` builds the guest (`nix build .#microvmImage` → `./result`) and runs
the CLI with `AGENTVM_RUNNER` pointing at it.

### Installing from the flake

The flake's package wraps the binary with `virtiofsd` on `PATH` and
`AGENTVM_RUNNER` baked in, so `agentvm` works from any directory.

**Imperatively** (user profile):

```sh
nix profile add github:judiantara/agentvm        # or `make install` from this repo
nix profile upgrade agentvm                           # after updating the flake
```

Usage is then simply:

```sh
cd ~/some/project
agentvm --resume
```

**Without installing:**

```sh
cd ~/some/project
nix run github:judiantara/agentvm -- --resume
```

### Authentication

Either log in through Claude Code's normal sign-in flow on first run, or use an
API key. No host variables reach the guest by default, so the key must be
forwarded explicitly: build the package with
`pkgs.agentvm.config { forwardEnv = [ "ANTHROPIC_API_KEY" ]; }` (or set
`AGENTVM_FORWARD_ENV=ANTHROPIC_API_KEY` with `make run`) and export it on the
host. The login is stored in `.agentvm/` (see below), so it survives between runs
of the same project.

### Exiting

Quit Claude Code normally (`/exit` or Ctrl-D). The VM powers off and `agentvm` cleans up.
Ctrl-C is delivered to the VM, not to `agentvm` cli.
If the VM is ever stuck, run `pkill cloud-hypervisor` from another terminal.

## Files created in your project

| Path | Purpose |
|---|---|
| `.agentvm/` | Guest user's `$HOME`: Claude Code login, settings, session history (`.agentvm/.claude/`, `.agentvm/.claude.json`) |

Keep `.agentvm/` out of version control (e.g. add it to `.gitignore`) — it can
contain credentials. The project's own `.claude/` directory (project settings,
commands) is left untouched and is read by Claude Code as usual.

Sessions are stored per directory, and the guest always sees the project as
`/work`, so `--resume` lists the sessions of the project you launched from.

## How it works

```
host                                               guest (microVM, CID 3)
────                                               ──────────────────────
agentvm
 ├─ virtiofsd  ── $PWD ──── vhost-user ─────────►  /work  (virtiofs, uid map)
 ├─ HTTP proxy ◄── notify.vsock_3128 ◄── vsock ──  socat 127.0.0.1:3128 ◄─ HTTPS_PROXY
 ├─ env/args   ── notify.vsock_3129 ─── vsock ──►  session script (eval)
 └─ microvm-run (cloud-hypervisor)
      --console tty  ◄═══ your terminal ═══►       hvc0 ── agentvm-session.service ── claude
      --serial file=console.log                    ttyS0 (kernel/boot log)
```

1. **Run directory.** `agentvm` creates `$TMPDIR/agentvm-<pid>/`. The microvm.nix
   runner resolves its sockets relative to its working directory, so it is
   started there and finds `agentvm-virtiofs-work.sock`, `notify.vsock`,
   `agentvm.sock` (API) and `console.log` in it. The directory is removed on exit.
2. **Shared directory.** `agentvm` starts `virtiofsd` on the current directory.
   `--translate-uid/--translate-gid` map the guest user (uid 1000, gid 1000)
   onto your host uid/gid, so files are owned correctly on both sides
   regardless of your host uid.
3. **Network.** The guest has no NIC. A socat service in the guest listens on
   `127.0.0.1:3128` and forwards to the host (vsock CID 2, port 3128).
   cloud-hypervisor delivers guest-initiated vsock connections on port *N* to
   the Unix socket `notify.vsock_N`, where `agentvm` runs a minimal HTTP proxy
   (CONNECT tunnels plus plain HTTP). The session sets `HTTP(S)_PROXY`
   accordingly. Anything that ignores the proxy simply has no network.
4. **Environment and arguments.** On vsock port 3129 `agentvm` serves shell
   `export` lines for allowlisted variables plus a `set -- …` line with its own
   command-line arguments, all single-quoted. The session script `eval`s this
   and runs `exec claude "$@"`. Secrets never appear on a command line or in the
   VM image.
5. **Console.** cloud-hypervisor attaches the guest's virtio console to your
   terminal (`--console tty`), including window-size changes. The
   `agentvm-session` systemd service runs `claude` on `/dev/hvc0` as user `agentvm`
   in `/work`, with `HOME=/work/.agentvm`. The kernel log goes to the serial port,
   which is written to `console.log`.
6. **Shutdown.** When `claude` exits, the service's `ExecStopPost` powers the
   guest off, cloud-hypervisor exits, and `agentvm` stops virtiofsd and deletes
   the run directory.

### Why `HOME` is not the project current directory

Claude Code does not remember folder trust for the home directory, so with
`HOME=/work` it would ask on every start; it would also mix `~/.claude` (user
data) with the project's own `.claude/`. Using `/work/.agentvm` avoids both while
keeping state inside the shared directory.

## Troubleshooting

- **`AGENTVM_DEBUG=1`** prints the last 40 lines of the guest console after the
  session ends. While running, `tail -f /tmp/agentvm-*/console.log` shows it live.
- **`virtiofsd did not create its socket`** — virtiofsd failed to start; run
  it by hand with the same arguments to see why (an old virtiofsd may lack
  `--translate-uid`).
- **Terminal garbled after a crash** — run `reset`.
- **Network errors inside the guest** — the tool must honour `HTTPS_PROXY`;
  there is no other route out.
- virtiofsd warnings about file-descriptor limits, setting uid/gid, or file
  handles are expected when running unprivileged and are harmless.

## Development

```sh
devenv shell        # Rust toolchain, cloud-hypervisor, virtiofsd, socat
make vm             # build the guest runner into ./result-vm
make cli            # cargo build --release
make clean
```
