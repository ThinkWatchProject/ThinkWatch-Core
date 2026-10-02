<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/brand/logo-dark.png">
    <img src="docs/brand/logo.png" alt="ThinkWatch Core" width="580">
  </picture>
</p>

<p align="center">
  <img src="https://img.shields.io/badge/Rust-000000?style=for-the-badge&logo=rust&logoColor=white" alt="Rust" />
  <img src="https://img.shields.io/badge/License-MIT-750014?style=for-the-badge" alt="License: MIT" />
  <img src="https://img.shields.io/badge/arm64-555555?style=for-the-badge&label=macOS&labelColor=000000&logo=apple&logoColor=white" alt="macOS: arm64" />
  <img src="https://img.shields.io/badge/x64%20%7C%20arm64-555555?style=for-the-badge&label=Windows&labelColor=0078D4" alt="Windows: x64, arm64" />
  <img src="https://img.shields.io/badge/x86__64%20%7C%20aarch64-555555?style=for-the-badge&label=Linux&labelColor=FCC624&logo=linux&logoColor=black" alt="Linux: x86_64, aarch64" />
</p>

# ThinkWatch Core

**[English](README.md) | [中文](README.zh-CN.md)**

ThinkWatch Core 是 ThinkWatch 的网关引擎，由一组 Rust crate 及其构建的 `twcore` 二进制组成。Claude Code、Codex 以及其他使用 Anthropic、OpenAI、Gemini 接口的客户端只需指向 `twcore` 一次；请求发出前可替换其中的凭据，上游返回的工具调用经过审查，每个请求的去向与费用都有记录。`twcore` 既运行在桌面应用 [ThinkWatch Lite](https://github.com/ThinkWatchProject/ThinkWatch-Lite) 中，也可以独立部署在 Linux 服务器上；[ThinkWatch 企业版](https://github.com/ThinkWatchProject/ThinkWatch)建立在其中四个 crate 之上。

文档：[配置手册](docs/config.zh-CN.md) · [在服务器上运行 core](docs/server.zh-CN.md) · [thinkwat.ch/zh-CN/core](https://thinkwat.ch/zh-CN/core/)

## 主要特性

- **一次接入，随时切换**。客户端只保留一个地址和一把密钥，更换上游或模型都在网关中完成，客户端无需改配置或重启。Anthropic Messages、OpenAI Chat Completions、OpenAI Responses 与 Gemini 四种格式双向转换，流式输出同样适用。
- **出站脱敏**。出站脱敏可在请求发出前把 API 密钥、私钥和连接串中的口令替换为占位符，并在回答回显时还原，中转站因此看不到真实的值。
- **拦截恶意工具调用**。中转站可以改写回答，塞入让客户端执行的工具调用。回答中的工具调用若是下载即执行、外发环境变量或凭据文件、读取私钥、写入开机启动项或定时任务，工具调用审查可以在客户端收到完整调用之前切断回答；另有隐藏字符检测、内容过滤和输出长度，共五项防护。出厂时除输出长度为关闭外均为观察档，切换到拦截档之前不改变任何请求。
- **每个请求都可追溯**。每个请求连同决定其去向的规则、每次尝试、格式转换、用量、费用及价格来源、首 token 时间和生成速度一并保存。试算可以在不发出请求的情况下说明请求会被送往何处；已保存的请求可以对另一个上游重放，以便对比。
- **路由与故障转移**。规则可按模型、密钥、格式、请求大小、工具、图片、思考等条件匹配，把请求交给一个上游或策略组（按顺序、手动指定、轮流、最低延迟、最低价格）。响应的首字节到达客户端之前，失败的上游由下一个候选替换，并按其给出的失败原因暂停相应的时间。
- **多种上游**。服务商的 API 密钥、任意兼容接口、OpenRouter 等中转站、本地模型、Amazon Bedrock，以及 ChatGPT 和 Z.ai 账号。连通性检查和预热请求默认在本地应答，不产生费用。
- **如实计费**。用量（含缓存读写）按每日刷新的公开价目表或自定义价目表计价。估算的费用会标明，无法计价的用量单独统计，不计作零。
- **单一配置，即改即生效**。全部设置保存在 `config.yaml` 中；无论改动来自编辑器、命令行还是控制面，通过校验后一秒内生效，最近五十个版本均可恢复。

## 安装

**桌面**。[ThinkWatch Lite](https://github.com/ThinkWatchProject/ThinkWatch-Lite) 自带 `twcore`，并随应用一同更新，无需另行下载。

**Linux 服务器**（x86_64 或 aarch64，glibc 2.35 及以上：Ubuntu 22.04、Debian 12 或更新）。一条命令即可把 `twcore` 安装为使用专用用户的 systemd 服务：

```sh
curl -fsSL https://raw.githubusercontent.com/ThinkWatchProject/ThinkWatch-Core/main/scripts/install.sh | sudo sh
```

脚本不会启动服务。之后以服务用户的身份执行：

```sh
alias twc='sudo -u thinkwatch THINKWATCH_HOME=/var/lib/thinkwatch twcore'

twc remote enable --allow 192.168.1.0/24   # 对这个网段开放远程控制端口
twc check                                  # 校验 config.yaml
sudo systemctl enable --now twcore         # 立即启动服务，并随系统启动
twc control-key                            # 标准输出是 ThinkWatch Lite 所需的密钥，地址和端口在标准错误
```

在 ThinkWatch Lite 中打开 **设置 → 连接 → 添加远程连接**，填写地址、控制端口和密钥。应用只连接与其内置版本相同的 core，`sudo twcore upgrade --version <版本> --restart` 可以把服务器切换到该版本。两个端口都不使用 TLS，只应对可信网络开放。密钥的存放、网络暴露、升级与卸载见[在服务器上运行 core](docs/server.zh-CN.md)。

**预编译二进制**：每个 [Release](https://github.com/ThinkWatchProject/ThinkWatch-Core/releases/latest) 都提供 macOS（Apple silicon）、Windows（x64、ARM64）和 Linux（x86_64、aarch64）版本，均附 `.sha256` 校验文件；Linux 的 `.tar.gz` 包含 systemd 服务单元。

`twcore` 的配置和数据存放在 `~/.thinkwatch`（Windows 上为 `%APPDATA%\ThinkWatch`），设置了 `THINKWATCH_HOME` 时存放在它指定的目录。每个字段的说明见[配置手册](docs/config.zh-CN.md)。

## 控制面

控制面是运行在加密通道内的 HTTP 接口，经 unix socket（Windows 上为回环端口）以及可选的远程端口访问。每条连接都以 `Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s` 握手开始，密钥为 `listen.control.key`，不涉及证书。curl 无法访问控制面，需使用 `twcore call`：

```
twcore call /status
twcore call -X POST -d '{"model":"claude-sonnet-4-5","route":"default"}' /dryrun
twcore control-key --rotate     # 更换密钥；用旧密钥建立的连接随即断开
```

远程端口只接受 `allow_from` 中的来源，最多同时保持 32 条连接，且不能停止 core、生成诊断包或修改 `listen.control`。

## crate 一览

| crate | 职责 |
|---|---|
| `tw-dialect` | 四种接口格式之间的转换，用量解析 |
| `tw-guard` | 五项防护：出站脱敏、工具调用审查、隐藏字符、内容过滤、输出长度 |
| `tw-breaker` | 熔断状态机 |
| `tw-bedrock` | Amazon Bedrock 的线上处理：SigV4 签名、eventstream、地址、模型目录 |
| `tw-types` | 给人看的消息：稳定的消息码、参数与英文句子 |
| `tw-engine` | 路由规则与策略组 |
| `tw-pricing` | 公开价目表、自定义价目表，实测／估算／无法计价三种费用 |
| `tw-yaml` | 在 YAML 原文上做最小改动 |
| `tw-secret` | 从环境变量或命令取得凭据，以及打码 |
| `tw-watch` | 带防抖的目录监视 |
| `tw-api` | 控制面契约：类型与客户端 |
| `tw-link` | 控制通道的握手与加密 |
| `tw-config` | 配置的结构、加载与校验 |
| `tw-store` | 基于 SQLite 的请求记录与运行时状态 |
| `tw-observe` | 事件总线 |
| `tw-gateway` | 数据面：一个请求的完整生命周期 |
| `tw-plugin` | 脚本插件的沙箱：编成 WebAssembly 的 QuickJS，由 Wasmtime 运行 |
| `tw-control` | 控制面服务 |

ThinkWatch 企业版只依赖前四个 crate，这四个 crate 也只相互依赖；CI 会针对它们的每一次改动检查企业版能否编译。ThinkWatch Lite 把 `tw-api`、`tw-types`、`tw-yaml`、`tw-guard`、`tw-watch` 和 `tw-link` 固定在某个 Release 的 tag 上，并打包同一 Release 的 `twcore`。接管 AI 客户端和扫描其配置在 Lite 中完成，作用于应用所在的机器；`twcore` 只为每个客户端签发专用的网关密钥。二进制的源码位于 `bin/twcore`。只有 `tw-gateway` 和 `twcore` 可以依赖 `tw-plugin`——它是唯一一个构建时除了 Rust 还需要别的工具的 crate（见下文），因此用这些 crate 构建 Lite 或企业版时都不需要。

## 构建与测试

需要较新的 Rust 稳定版工具链（1.94.1 或更新），以及一个能把 C 编译成 WebAssembly 的 LLVM `clang` 和与之配套的 `llvm-ar`。插件沙箱（`tw-plugin`）在构建时把 QuickJS 编译成 WebAssembly；Apple 自带的 clang 不支持 WebAssembly。

| 系统 | 安装 |
|---|---|
| macOS | `brew install llvm`（构建时会在 Homebrew 的安装位置找到它，不必加入 `PATH`） |
| Debian、Ubuntu | `sudo apt install clang llvm` |
| Fedora | `sudo dnf install clang llvm` |
| Windows | [LLVM 发布页](https://github.com/llvm/llvm-project/releases)上的安装包，或 `winget install LLVM.LLVM` |

构建时依次尝试 Homebrew 的 LLVM、`PATH` 上的 `clang` 和 `clang-N`（`clang-19`、`clang-18`……），使用第一个确实能产出 WebAssembly 的。要指定某一个，设置 `TW_WASM_CLANG`；它的 `llvm-ar` 不在同一目录时，再设置 `TW_WASM_AR`。Rust 的 `wasm32-unknown-unknown` 目标已写在 `rust-toolchain.toml` 中，rustup 会自动安装；链接使用 Rust 自带的 `rust-lld`。构建过程中不下载任何东西。每次构建都会记录所用的 clang 和 WebAssembly 模块的 SHA-256（`tw_plugin::GUEST_CLANG`、`tw_plugin::GUEST_WASM_SHA256`）。

```sh
cargo build --release -p twcore     # 生成 target/release/twcore
cargo run -p twcore -- init         # 生成初始的 config.yaml
cargo run -p twcore -- serve        # 启动网关和控制面
cargo test --workspace              # 单元测试与集成测试
scripts/smoke.sh                    # 在临时 HOME 中用真实的二进制把每条路径运行一遍
```

提交 PR 前须通过的检查，以及配置手册、价目表和发版流程的说明，见 [CONTRIBUTING.md](CONTRIBUTING.md)。

## 许可证

[MIT](LICENSE)
