# sbc

`sbc` connects your terminal to a Docker Sandbox—on this machine or over SSH.
It can reconnect to the sandbox's coding agent, open a shell, or run any other
interactive command. `Ctrl+V` works across the sandbox boundary: text pastes as
text, while screenshots are copied into the sandbox and inserted as a readable
path.

```console
$ sbc config set-host build-server
$ sbc my-task

# Or open a shell instead of the agent session
$ sbc my-task -- bash
```

## Install

macOS, Linux, or WSL:

```sh
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/wigentic-ai/sbc/releases/latest/download/sbc-installer.sh | sh
```

Windows PowerShell:

```powershell
irm https://github.com/wigentic-ai/sbc/releases/latest/download/sbc-installer.ps1 | iex
```

You need Docker's `sbx` CLI on the machine that hosts the sandbox and OpenSSH
for remote connections.

## Use

```console
# Default host, when configured
$ sbc my-task

# Explicit SSH host
$ sbc build-server/my-task

# Local sandbox
$ sbc local/my-task

# Any interactive command
$ sbc my-task -- bash
$ sbc my-task -- python
```

Press `Ctrl+V` inside a connected session. If the clipboard contains an image,
`sbc` streams it into sandbox `/tmp` and inserts a marker such as:

```text
[image: /tmp/sbc/sbc-1234.png]
```

Temporary images are removed when the session ends. Use `--no-clipboard` to
pass `Ctrl+V` through untouched.

Set the default remote host once:

```console
$ sbc config set-host build-server
```

Host names use your normal SSH configuration. Run `sbc config path` to find the
config file; advanced host aliases can override `ssh_host` or `sbx_command`:

```toml
default_host = "build"

[hosts.build]
ssh_host = "build-server"
sbx_command = "/usr/bin/sbx"
```

`sbc` does not create, register, or retain sandboxes. Docker's `sbx` remains the
control plane.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). `sbc` is available under the
[MIT License](LICENSE).
