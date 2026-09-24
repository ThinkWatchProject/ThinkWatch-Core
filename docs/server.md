# Running core on a server

[中文](server.zh-CN.md)

ThinkWatch Core runs without a desktop: on a Linux machine it is started by
systemd from its configuration file, and the ThinkWatch Lite app on a Mac
connects to it over the network to show traffic and change settings. Clients
anywhere on the network send their requests to the server's gateway.

This page covers installing, configuring, starting, connecting and
upgrading. Every field mentioned is described in the
[configuration reference](config.md).

## Requirements

- Linux on x86_64 or aarch64, with glibc 2.35 or newer (Ubuntu 22.04,
  Debian 12, or later).
- systemd.
- The server's core version has to match the desktop app's. The app checks
  this when it connects and shows both versions if they differ.

## 1. Install

```sh
curl -fsSL https://raw.githubusercontent.com/ThinkWatchProject/ThinkWatch-Core/main/scripts/install.sh | sudo sh
```

To install a particular version, the one your desktop app expects:

```sh
curl -fsSL https://raw.githubusercontent.com/ThinkWatchProject/ThinkWatch-Core/main/scripts/install.sh | sudo sh -s -- --version 0.47.0
```

The script:

1. downloads `twcore-<arch>-unknown-linux-gnu.tar.gz` from the GitHub
   release and checks it against the release's SHA-256 sum;
2. installs the binary as `/usr/local/bin/twcore`;
3. creates the system user `thinkwatch` and the data directory
   `/var/lib/thinkwatch` (mode `0700`);
4. installs `/etc/systemd/system/twcore.service` and an empty
   `/etc/thinkwatch/env`;
5. runs `twcore init` as `thinkwatch` if there is no configuration yet;
6. prints the next steps. It does not start the service.

Running it again is safe: it replaces the binary and the unit, and leaves
the configuration, the environment file and the data alone. For later
upgrades, `twcore upgrade` is the shorter way (see below).

To install by hand instead, download the tarball and its `.sha256` from the
[releases page](https://github.com/ThinkWatchProject/ThinkWatch-Core/releases),
check it with `sha256sum -c`, and follow the steps above; the unit file is
in the tarball and in [`packaging/systemd/twcore.service`](../packaging/systemd/twcore.service).

Every `twcore` command that reads the configuration has to run as the
service user with the service's data directory. The examples below spell
that out; a shell alias saves typing:

```sh
alias twc='sudo -u thinkwatch THINKWATCH_HOME=/var/lib/thinkwatch twcore'
```

## 2. Configure

Open `/var/lib/thinkwatch/config.yaml` as root (`sudoedit` works) and change
three things:

1. **Let clients on the network reach the gateway**: `listen.gateway.bind:
   all`, and list their networks in `listen.gateway.allow_from`.
2. **Open the remote control port**: `listen.control.remote.enabled: true`,
   and list the networks the desktop app connects from in its
   `allow_from`. `twcore init` writes this section with a random port and
   `enabled: false`; if your file has no `remote` section, add one with any
   free port.
3. **Add at least one upstream** under `providers`, or add it later from the
   desktop app.

```yaml
version: 1
listen:
  gateway:
    bind: all
    port: 8788
    allow_from: [192.168.1.0/24]
  control:
    key: 9f2c…e41a            # written by twcore init; leave it as it is
    remote:
      enabled: true
      bind: all
      port: 41327             # written by twcore init
      allow_from: [192.168.1.0/24]
clients:
  - name: default
    key: tw-…                 # written by twcore init
providers:
  - name: anthropic
    base_url: https://api.anthropic.com
    key: ${ANTHROPIC_API_KEY}
```

Check the result without starting anything:

```sh
sudo -u thinkwatch THINKWATCH_HOME=/var/lib/thinkwatch twcore check
```

### Secrets in the environment

`${NAME}` in the configuration reads the environment of the core process.
Under systemd that is `/etc/thinkwatch/env`, one `NAME=value` per line:

```sh
sudoedit /etc/thinkwatch/env      # created by the installer: root:thinkwatch, 0640
```

```ini
ANTHROPIC_API_KEY=sk-ant-…
HTTPS_PROXY=http://proxy.example.com:3128
```

The file is read when the service starts: after changing it, restart the
service. Proxy variables here are what `proxy: system` uses.

### Network

Neither port has TLS. The control port is encrypted and authenticated by
its handshake; the gateway port carries requests in plain HTTP, like any
local model server. Keep both reachable only from networks you trust: set
`allow_from`, and open the two ports in the server's firewall to those
networks only. For access from outside, use a VPN or an SSH tunnel rather
than exposing the ports.

## 3. Start

```sh
sudo systemctl enable --now twcore
systemctl status twcore
journalctl -u twcore -f
```

The unit runs core as `thinkwatch`, restarts it if it fails, and keeps it
out of the rest of the file system: it can write only its data directory.

Configuration changes need no restart. Core reloads the file within a
second of a save, from an editor, `twcore config`, or the desktop app; a
change that does not validate is refused and the previous configuration
keeps serving.

## 4. Connect the desktop app

Show the control key on the server:

```sh
sudo -u thinkwatch THINKWATCH_HOME=/var/lib/thinkwatch twcore control-key
```

In the desktop app, open **Settings → Connections → Add remote connection**
and enter:

- **Address**: the server's host name or IP address;
- **Control port**: `listen.control.remote.port`;
- **Key**: the 64 characters `twcore control-key` printed.

The app tests the connection before saving and says what is wrong if it
fails: no answer (address, port, firewall, `enabled`), connection closed
(this Mac's address is probably not in `allow_from`), wrong key, or
different versions.

The key is kept in the Mac's keychain. To replace it, run
`twcore control-key --rotate` on the server; connected apps then have to be
given the new key.

### Point clients at the server

Clients use the server's gateway, `http://<server>:8788`, with a gateway key
from `clients`. The desktop app can point the clients on the Mac at the
server (Clients page); on other machines, configure them by hand.

## Upgrading

```sh
sudo twcore upgrade --check       # compare with the latest release, change nothing
sudo twcore upgrade --restart     # install the latest release and restart the service
sudo twcore upgrade --version 0.48.0 --restart
```

`twcore upgrade` downloads the release for this machine, checks its
SHA-256 sum, and replaces `/usr/local/bin/twcore` in one step, so a failed
download never leaves a broken binary. The configuration and the data are
not touched. Without `--restart` it prints the command to restart the
service; the running process keeps the old version until then.

Upgrade the server and the desktop app together: the app refuses to connect
to a core of another version and shows the command above with the version
it needs.

## Uninstalling

```sh
sudo systemctl disable --now twcore
sudo rm /etc/systemd/system/twcore.service /usr/local/bin/twcore
sudo systemctl daemon-reload
# The configuration, keys and request history:
sudo rm -r /var/lib/thinkwatch /etc/thinkwatch
sudo userdel thinkwatch
```
