<p align="center">
  <img src="https://img.shields.io/badge/Rust-000000?style=for-the-badge&logo=rust&logoColor=white" alt="Rust" />
  <img src="https://img.shields.io/badge/License-MIT-750014?style=for-the-badge" alt="License: MIT" />
  <img src="https://img.shields.io/badge/arm64-555555?style=for-the-badge&label=macOS&labelColor=000000&logo=apple&logoColor=white" alt="macOS: arm64" />
  <img src="https://img.shields.io/badge/x64%20%7C%20arm64-555555?style=for-the-badge&label=Windows&labelColor=0078D4" alt="Windows: x64, arm64" />
  <img src="https://img.shields.io/badge/x86__64%20%7C%20aarch64-555555?style=for-the-badge&label=Linux&labelColor=FCC624&logo=linux&logoColor=black" alt="Linux: x86_64, aarch64" />
</p>

# ThinkWatch Core

**[English](README.md) | [中文](README.zh-CN.md)**

ThinkWatch Core 是一组 Rust crate，以及由它们构建的网关二进制 `twcore`。Claude Code、Codex 以及其他使用 Anthropic、OpenAI、Gemini 接口的客户端经由 `twcore` 发出请求；它按规则路由每个请求，在响应开始前故障转移到其他上游，记录每个请求的费用，在请求发出前脱敏其中的密钥，并审查上游返回的工具调用。

`twcore` 既作为本机网关运行在桌面应用 [ThinkWatch Lite](https://github.com/ThinkWatchProject/ThinkWatch-Lite)（macOS、Windows、Linux）中，也可以作为独立网关部署在 Linux 服务器上，由 ThinkWatch Lite 通过加密的控制通道连接。[ThinkWatch 企业版](https://github.com/ThinkWatchProject/ThinkWatch)依赖其中三个 crate：`tw-dialect`、`tw-guard` 和 `tw-breaker`。

文档：[配置手册](docs/config.zh-CN.md) · [在服务器上运行 core](docs/server.zh-CN.md) · [thinkwat.ch/zh-CN/core](https://thinkwat.ch/zh-CN/core/)

## 功能

- **按规则路由**。具名规则可按以下条件匹配：请求的模型（支持通配符）、网关密钥、接口格式、估算的输入 token 数、`max_tokens`，以及请求是否使用工具、图片、扩展思考、提示缓存或流式输出。命中的规则把请求交给一个上游或策略组、改写参数（模型、`max_tokens`、思考），或附带原因拒绝请求。策略组按顺序（`fallback`）、手动指定（`select`）、轮流（`load-balance`）、按实测首字节时间（`url-test`）或按价格（`cheapest`）选择上游。
- **故障转移**。响应的首字节到达客户端之前，失败的上游由下一个候选替换，客户端不会察觉，每次尝试都随请求记录。在此之后中断的流以一个错误事件结束，客户端不会把它当作完整的回答。连续失败三次的上游暂停使用一分钟，之后再次尝试。
- **格式转换**。客户端可以使用 Anthropic Messages、OpenAI Chat Completions、OpenAI Responses 或 Gemini 格式，与上游使用的格式无关；两者不同时，请求、响应和流式输出都会转换。上游可以是服务商的 API、OpenRouter 等中转站、本地模型服务或 ChatGPT 账号。
- **费用核算**。每个请求的 token 用量（含缓存读写）按每日刷新的公开价目表计价；收费方式不同的上游可以选用自定义价目表（倍率或逐模型价格），本地模型等上游可以设为不计费。无法计价的用量标为「未知」，不计作零；每个请求保存其费用及价格来源。
- **防护**。五项防护作用于所有请求，不区分上游，各有关闭、观察、拦截三档。在拦截档下：出站脱敏在请求发出前把 API 密钥、私钥、连接串中的口令等凭据替换为占位符，并在响应回显时还原；工具调用审查在上游返回的工具调用命中危险命令规则时可以切断响应；隐藏字符检测拒绝带有 Unicode 标签字符或双向控制符的请求；内容过滤可以拒绝命中关键词或正则规则的请求；输出长度在回答超过设定的字符数时将其切断。在观察档下，防护只记录发现的内容，不改变任何行为。除输出长度出厂为关闭外，其余各项出厂均为观察。内置规则可以逐条停用，也可以添加自定义规则。
- **请求记录与实时事件**。每个请求连同命中的规则、每次上游尝试、用量和费用保存在本机的 SQLite 数据库中。控制面在请求开始和结束时推送事件；试算可以在不发出请求的情况下说明请求会被路由到哪里及其原因。
- **单一配置文件**。全部设置保存在 `config.yaml` 中。无论改动来自编辑器、`twcore config` 还是控制面，通过校验后都在一秒内生效；未通过校验的改动被拒绝，原配置继续生效。最近五十个版本都会保留，可以恢复其中任一版本。

## 安装

### 预编译二进制

每个 [Release](https://github.com/ThinkWatchProject/ThinkWatch-Core/releases/latest) 都提供以下五个平台的 `twcore`，每个文件都附带 `.sha256` 校验文件：

| 平台 | 文件 |
|---|---|
| macOS，Apple silicon | `twcore-aarch64-apple-darwin` |
| Windows，x64 | `twcore-x86_64-pc-windows-msvc.exe` |
| Windows，ARM64 | `twcore-aarch64-pc-windows-msvc.exe` |
| Linux，x86_64 | `twcore-x86_64-unknown-linux-gnu`，或附带 systemd 服务单元的 `twcore-x86_64-unknown-linux-gnu.tar.gz` |
| Linux，aarch64 | `twcore-aarch64-unknown-linux-gnu`，或附带 systemd 服务单元的 `twcore-aarch64-unknown-linux-gnu.tar.gz` |

校验下载时，在两个文件所在的目录中执行 `sha256sum -c <文件>.sha256`（macOS 上为 `shasum -a 256 -c <文件>.sha256`）。Linux 版本需要 glibc 2.35 或更新（Ubuntu 22.04、Debian 12 及以后）。桌面上无需单独下载：ThinkWatch Lite 自带 `twcore`，并随应用一同更新。

### Linux 服务器

```sh
curl -fsSL https://raw.githubusercontent.com/ThinkWatchProject/ThinkWatch-Core/main/scripts/install.sh | sudo sh
```

脚本把 `twcore` 安装为 systemd 服务。后续步骤概述于[服务器部署](#服务器部署)，完整说明见[在服务器上运行 core](docs/server.zh-CN.md)。

### 从源码构建

需要 Rust 稳定版工具链，1.85 或更新。

```sh
git clone https://github.com/ThinkWatchProject/ThinkWatch-Core.git
cd ThinkWatch-Core
cargo build --release -p twcore     # 生成 target/release/twcore
```

在源码目录中直接运行：

```sh
cargo run -p twcore -- init     # 生成初始的 config.yaml
cargo run -p twcore -- check    # 只校验配置，不启动
cargo run -p twcore -- serve    # 启动网关和控制面
```

`twcore` 的配置和数据存放在 `~/.thinkwatch`（Windows 上为 `%APPDATA%\ThinkWatch`），设置了 `THINKWATCH_HOME` 时存放在它指定的目录。`config.yaml` 的每个字段见[配置手册](docs/config.zh-CN.md)。

## 服务器部署

twcore 可以作为 systemd 服务运行在 Linux（x86_64、aarch64）上。一条命令即可安装二进制、专用用户和服务单元；各平台的 ThinkWatch Lite 通过远程控制端口连接它。

```sh
curl -fsSL https://raw.githubusercontent.com/ThinkWatchProject/ThinkWatch-Core/main/scripts/install.sh | sudo sh
```

脚本按 SHA-256 校验下载的文件，安装 `/usr/local/bin/twcore`，创建系统用户 `thinkwatch` 及其数据目录 `/var/lib/thinkwatch`，并安装 `twcore.service`。尚无配置时，脚本以该用户执行 `twcore init`，生成的 `config.yaml` 包含一把网关密钥、控制密钥，以及处于关闭状态的远程控制端口（端口号在 20000–32000 之间随机选取）。脚本不会启动服务。安装指定版本时，在 `sudo sh` 后加上 `-s -- --version 0.47.0`。

读取配置的命令须以服务用户的身份、使用服务的数据目录执行：

```sh
alias twc='sudo -u thinkwatch THINKWATCH_HOME=/var/lib/thinkwatch twcore'

twc remote enable --allow 192.168.1.0/24   # 对这个网段开放远程控制端口
twc check                                  # 校验 config.yaml
sudo systemctl enable --now twcore         # 立即启动服务，并随系统启动
twc control-key                            # 标准输出是 ThinkWatch Lite 所需的密钥，地址和端口在标准错误
```

要让其他机器上的客户端使用网关，在 `config.yaml` 中设置 `listen.gateway.bind: all`，并在 `listen.gateway.allow_from` 中列出它们所在的网段。上游可以写在 `providers` 下，也可以之后在 ThinkWatch Lite 中添加。在 ThinkWatch Lite 中打开 **设置 → 连接 → 添加远程连接**，填写服务器地址、控制端口和密钥。应用只连接与自身版本一致的 core，因此服务器与应用须一同升级：

```sh
sudo twcore upgrade --check                      # 与最新 Release 比对，不做任何改动
sudo twcore upgrade --restart                    # 安装最新 Release 并重启服务
sudo twcore upgrade --version 0.47.0 --restart   # 安装指定版本
```

两个端口都不使用 TLS：控制端口由握手完成加密和鉴权，网关端口以明文 HTTP 传输请求，因此两者都只应对可信网络开放。完整步骤见[在服务器上运行 core](docs/server.zh-CN.md)，包括 `/etc/thinkwatch/env` 中的密钥、网络暴露和卸载；`config.yaml` 的每个字段见[配置手册](docs/config.zh-CN.md)。

## crate 分层

工作区共有十六个 crate，分为两层；`twcore` 二进制位于 `bin/twcore`。

| 层 | crate |
|---|---|
| 与 ThinkWatch 企业版共用 | `tw-dialect` · `tw-guard` · `tw-breaker` |
| 领域逻辑 | `tw-types` · `tw-engine` · `tw-pricing` · `tw-yaml` · `tw-secret` · `tw-watch` |
| 装配 | `tw-config` · `tw-store` · `tw-observe` |
| 数据面与控制面 | `tw-gateway` · `tw-control` · `tw-api` · `tw-link` |

第一行是共用层；其余各行组成第二层，即网关本身。

ThinkWatch 企业版只依赖共用层的这三个 crate：格式转换与用量解析（`tw-dialect`），脱敏、工具调用审查及其他防护（`tw-guard`），以及熔断状态机（`tw-breaker`）。这三个 crate 只相互依赖，由测试保证；CI 会针对它们的每一次改动检查 ThinkWatch 企业版能否编译。只有一方使用的组件放在那一方的仓库中。

ThinkWatch Lite 把 `tw-api`、`tw-types`、`tw-yaml`、`tw-guard`、`tw-watch` 和 `tw-link` 固定在某个 Release 的 tag 上，并打包同一 Release 的 `twcore`。接管 AI 客户端（把其配置指向网关）、编辑其 MCP 服务器和扫描其配置文件都在 ThinkWatch Lite 中完成：这些操作修改的是应用所在机器上的文件，而这台机器不一定运行着 `twcore`。`twcore` 只负责为每个客户端签发专用的网关密钥。

第二层实现的是单个 `twcore` 进程：请求记录保存在 SQLite 中，配置保存在一个 YAML 文件中，控制面经本机通道或远程控制端口访问。这一层有意不共用：ThinkWatch 企业版是多租户的，其状态保存在 PostgreSQL、Redis 和 ClickHouse 中，两种设计差异很大，用一个抽象同时覆盖两者，对双方都不合适。

## 控制面

控制面是运行在加密通道内的 HTTP 接口。它在 macOS 和 Linux 上监听 unix socket，在 Windows 上监听回环端口；可选的远程控制端口（`listen.control.remote`）另开一个网络监听，供其他机器上的 ThinkWatch Lite 连接。每种通道上的每条连接都以同一个握手开始：`Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s`（实现在 `tw-link` 中），密钥为 `config.yaml` 中的 `listen.control.key`，不涉及 TLS 证书。`twcore serve` 在开始监听前写入该密钥：新配置直接包含，已有的配置只补这一行。

HTTP 运行在加密通道内，curl 无法访问控制面，需使用 `twcore call`：

```
twcore control-key              # 打印 ThinkWatch Lite 连接所用的密钥
twcore control-key --rotate     # 更换密钥；用旧密钥建立的连接随即断开
twcore call /status
twcore call -X POST -d '{"model":"claude-sonnet-4-5","route":"default"}' /dryrun
```

控制面返回的配置原文中，密钥经过打码；经控制面的写入也无法修改它。

远程控制端口在本机通道之外另行开放，使用同一把密钥和同一个握手：

```
twcore remote enable --allow 192.168.1.0/24   # 端口在第一次开启时随机选取
twcore remote disable
twcore control-key                             # 标准输出是密钥，地址和端口在标准错误
```

来自 `allow_from` 之外的连接直接关闭，不回复任何字节；服务器本机的回环地址不会自动放行。同一来源一分钟内握手失败五次后，接下来一分钟内的连接一律忽略；远程连接最多同时存在 32 条。远程连接不能停止 core、不能生成诊断包，也不能修改 `listen.control`。

## 开发

```
cargo test --workspace     # 单元测试与集成测试
scripts/smoke.sh           # 从空白状态起，用真实的二进制把每条路径运行一遍
```

`scripts/smoke.sh` 不会改动用户自己的配置：`HOME` 和 `THINKWATCH_HOME` 都指向一个临时目录，运行结束后删除。提交 PR 前须通过的检查，以及配置手册、价目表和发版流程的说明，见 [CONTRIBUTING.md](CONTRIBUTING.md)。

## 许可证

[MIT](LICENSE)
