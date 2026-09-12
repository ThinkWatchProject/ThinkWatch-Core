<p align="center">
  <img src="https://img.shields.io/badge/Rust-000000?style=for-the-badge&logo=rust&logoColor=white" />
  <img src="https://img.shields.io/badge/License-MIT-750014?style=for-the-badge" />
  <img src="https://img.shields.io/badge/macOS-000000?style=for-the-badge&logo=apple&logoColor=white" />
</p>

# ThinkWatch Core

**[English](README.md) | [中文](README.zh-CN.md)**

**一个本地 AI API 网关的共用核心层。**路由、转发、观测、计价，以及一组
数据面的安全守卫 —— 桌面版（ThinkWatch Lite）和服务端版本都用它。

**这个仓库不是一个装上就能用的应用**，它是一组 crate。想要跑得起来的东西，
`bin/twcore` 是一个完整的、可独立运行的网关二进制。

```
cargo run -p twcore -- init     # 生成一份带注释的 config.yaml
cargo run -p twcore -- check    # 只校验，不启动
cargo run -p twcore -- serve    # 起网关和控制面
```

## 它做什么

把客户端（Claude Code、Codex 之类）指向本地的一个端口，然后：

- **按规则路由**到不同上游 —— 条件可以是模型名、客户端、上下文长度、
  有没有工具调用；动作是换上游、改参数、或者直接拒绝。
- **流式故障转移** —— 首字节之前可以透明换一家；首字节之后唯一诚实的
  做法是如实报告发生了什么。
- **看得见成本** —— token 用量、缓存命中、按内置价目表快照计价。
  **算不出价钱的明确标「未知」，而不是编一个数字。**
- **出站脱敏** —— 发给中转站之前，把请求里的密钥换成占位符；模型回显时
  再换回来。
- **入站审查** —— 上游返回的工具调用过一遍规则，高危的可以在那一帧上切断。

## crate 分层

```
tw-types · tw-protocol · tw-provider · tw-resil · tw-crypto   ← 形状由外部现实决定
tw-engine · tw-pricing · tw-redact · tw-yaml · tw-secret      ← 领域逻辑
tw-config · tw-store · tw-scan · tw-adopt · tw-observe        ← 装配
tw-gateway · tw-control                                       ← 数据面 / 控制面
```

上面两层对外部稳定，服务端版本直接依赖它们。下面两层是单机的实现
（SQLite、unix socket），**有意不共用** —— 单机 SQLite 和多租户 Postgres
差得太远，强行统一只会造出一个两边都别扭的抽象。

## 开发

```
cargo test --workspace     # 单元与集成测试
scripts/smoke.sh           # 从零起，在真二进制上把每条路走一遍
```

`scripts/smoke.sh` 不碰你自己的任何东西 —— `HOME` 和 `THINKWATCH_HOME`
都指向一个临时目录，跑完就删。

## License

MIT
