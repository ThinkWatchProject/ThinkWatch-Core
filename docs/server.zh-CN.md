# 在服务器上运行 core

[English](server.md)

ThinkWatch Core 可以脱离桌面运行：在 Linux 机器上由 systemd 按配置文件启动，Mac 上的 ThinkWatch Lite 通过网络连接它，查看流量、修改设置。网络中各处的客户端把请求发往服务器的网关。

本文依次说明安装、配置、启动、连接和升级。文中提到的每个字段，详见[配置手册](config.zh-CN.md)。

## 要求

- x86_64 或 aarch64 的 Linux，glibc 2.35 或更新（Ubuntu 22.04、Debian 12 及以后）。
- systemd。
- 服务器上的 core 版本须与桌面应用一致。应用在连接时核对版本，不一致时显示双方的版本号。

## 1. 安装

```sh
curl -fsSL https://raw.githubusercontent.com/ThinkWatchProject/ThinkWatch-Core/main/scripts/install.sh | sudo sh
```

安装指定版本（即桌面应用要求的版本）：

```sh
curl -fsSL https://raw.githubusercontent.com/ThinkWatchProject/ThinkWatch-Core/main/scripts/install.sh | sudo sh -s -- --version 0.47.0
```

安装脚本依次：

1. 从 GitHub Release 下载 `twcore-<架构>-unknown-linux-gnu.tar.gz`，并用 Release 中的 SHA-256 校验；
2. 把程序安装为 `/usr/local/bin/twcore`；
3. 创建系统用户 `thinkwatch` 和数据目录 `/var/lib/thinkwatch`（权限 `0700`）；
4. 安装 `/etc/systemd/system/twcore.service`，以及空的 `/etc/thinkwatch/env`；
5. 还没有配置时，以 `thinkwatch` 身份执行 `twcore init`；
6. 打印后续步骤。脚本不启动服务。

脚本可以重复执行：它替换程序和 unit 文件，不动配置、环境变量文件和数据。之后升级用 `twcore upgrade` 更简便（见下文）。

也可以手动安装：从 [Releases 页面](https://github.com/ThinkWatchProject/ThinkWatch-Core/releases)下载压缩包及其 `.sha256`，用 `sha256sum -c` 校验，再按上述步骤操作；unit 文件在压缩包中，也在 [`packaging/systemd/twcore.service`](../packaging/systemd/twcore.service)。

读取配置的 `twcore` 命令，都要以服务用户的身份、带上服务的数据目录执行。下文的示例都写全了；可以用一个 shell 别名简化：

```sh
alias twc='sudo -u thinkwatch THINKWATCH_HOME=/var/lib/thinkwatch twcore'
```

## 2. 配置

以 root 身份打开 `/var/lib/thinkwatch/config.yaml`（可用 `sudoedit`），修改三处：

1. **让网络中的客户端能访问网关**：`listen.gateway.bind: all`，并在 `listen.gateway.allow_from` 中列出客户端所在的网段。
2. **打开远程控制端口**：`listen.control.remote.enabled: true`，并在其 `allow_from` 中列出桌面应用所在的网段。`twcore init` 生成的配置带有这一节，`enabled: false`，端口是 20000 到 32000 之间随机的一个。也可以用命令完成，文件中没有这一节时命令会一并写出：

   ```sh
   sudo -u thinkwatch THINKWATCH_HOME=/var/lib/thinkwatch twcore remote enable --allow 192.168.1.0/24
   ```

   `--allow` 可以写多次，替换整个名单；`--bind`、`--port` 改网卡和端口。`twcore remote disable` 关闭端口，其余设置保留；`twcore remote` 查看当前状态。运行中的 core 在一秒内跟上。
3. **在 `providers` 下添加至少一个上游**，也可以之后在桌面应用中添加。

```yaml
version: 1
listen:
  gateway:
    bind: all
    port: 8788
    allow_from: [192.168.1.0/24]
  control:
    key: 9f2c…e41a            # twcore init 生成，保持原样
    remote:
      enabled: true
      bind: all
      port: 23483             # twcore init 随机生成
      allow_from: [192.168.1.0/24]
clients:
  - name: default
    key: tw-…                 # twcore init 生成
providers:
  - name: anthropic
    base_url: https://api.anthropic.com
    key: ${ANTHROPIC_API_KEY}
```

只校验、不启动：

```sh
sudo -u thinkwatch THINKWATCH_HOME=/var/lib/thinkwatch twcore check
```

### 用环境变量存放密钥

配置中的 `${NAME}` 读取 core 进程的环境变量。在 systemd 下，环境变量来自 `/etc/thinkwatch/env`，每行一个 `NAME=value`：

```sh
sudoedit /etc/thinkwatch/env      # 由安装脚本创建：root:thinkwatch，0640
```

```ini
ANTHROPIC_API_KEY=sk-ant-…
HTTPS_PROXY=http://proxy.example.com:3128
```

这个文件在服务启动时读取，修改后需要重启服务。其中的代理变量就是 `proxy: system` 使用的代理。

### 网络

两个端口都没有 TLS。控制端口由握手完成加密和鉴权；网关端口以明文 HTTP 传输请求，和本地模型服务一样。两个端口都只应对可信的网络开放：设置 `allow_from`，并在服务器防火墙中只对这些网段开放这两个端口。需要从外部访问时，使用 VPN 或 SSH 隧道，不要直接暴露端口。

## 3. 启动

```sh
sudo systemctl enable --now twcore
systemctl status twcore
journalctl -u twcore -f
```

unit 以 `thinkwatch` 身份运行 core，失败后自动重启，并把它与文件系统的其余部分隔开：它只能写自己的数据目录。

修改配置不需要重启。无论通过编辑器、`twcore config` 还是桌面应用保存，core 都会在一秒内重新加载；未通过校验的改动被拒绝，原有配置继续服务。

## 4. 连接桌面应用

在服务器上查看控制密钥：

```sh
sudo -u thinkwatch THINKWATCH_HOME=/var/lib/thinkwatch twcore control-key
```

```
9f2c…e41a
remote control: port 23483; connect to 192.168.1.20:23483
allowed sources: 192.168.1.0/24
```

第一行是密钥，也是标准输出上唯一的内容，脚本里 `$(twcore control-key)` 取到的就是密钥。后两行在标准错误上：端口、这台服务器上在该端口监听的网卡地址，以及放行的来源。端口关闭时第二行是 `remote control: off (twcore remote enable opens it)`。

在桌面应用中打开 **设置 → 连接 → 添加远程连接**，填写：

- **地址**：服务器的主机名或 IP 地址；
- **控制端口**：`listen.control.remote.port` 的值；
- **密钥**：`twcore control-key` 输出的 64 个字符。

应用在保存前先试连，失败时说明原因：无响应（检查地址、端口、防火墙和 `enabled`）、连接被关闭（本机地址可能不在 `allow_from` 中）、密钥不正确、版本不一致。这个端口的 `allow_from` 不会自动放行服务器本机；服务器上的命令走本地通道。

同一来源一分钟内握手失败五次，之后一分钟不理它。从 `allow_from` 中删掉一个网段，已经从那里连着的连接也随即断开。

密钥保存在 Mac 的钥匙串中。要更换密钥，在服务器上执行 `twcore control-key --rotate`：用旧密钥建立的连接立即断开，之后已连接的应用需要填入新密钥。

远程连接能做应用在本机能做的一切，只有三件事服务器会拒绝：停止 core（它由 systemd 管理）、生成诊断包、修改 `listen.control`（这条连接进来的那一节）。这三件事在服务器上操作。

### 让客户端指向服务器

客户端使用服务器的网关 `http://<服务器>:8788`，以及 `clients` 中的一把网关密钥。桌面应用可以把这台 Mac 上的客户端改为指向服务器（客户端页）；其他机器上的客户端需手动配置。

## 升级

```sh
sudo twcore upgrade --check       # 与最新版本比较，不做任何改动
sudo twcore upgrade --restart     # 安装最新版本并重启服务
sudo twcore upgrade --version 0.48.0 --restart
```

`twcore upgrade` 下载适合本机的版本，校验 SHA-256，一步替换 `/usr/local/bin/twcore`，下载失败也不会留下损坏的程序。配置和数据不受影响。不带 `--restart` 时只打印重启服务的命令；在重启之前，运行中的进程仍是旧版本。

服务器和桌面应用要一起升级：版本不一致时应用拒绝连接，并显示上面的命令及所需的版本。

## 卸载

```sh
sudo systemctl disable --now twcore
sudo rm /etc/systemd/system/twcore.service /usr/local/bin/twcore
sudo systemctl daemon-reload
# 配置、密钥和请求历史：
sudo rm -r /var/lib/thinkwatch /etc/thinkwatch
sudo userdel thinkwatch
```
