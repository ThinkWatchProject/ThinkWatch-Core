# ThinkWatch Core

一个本地 AI API 网关的核心层：路由、转发、观测、计价、以及一组数据面的
安全守卫。它同时被桌面版（ThinkWatch Lite）和服务端版本使用。

**这个仓库不是一个可以直接用的应用**，它是一组 crate。要跑起来的话，
`bin/twcore` 是一个完整的、可独立运行的网关二进制。

```
cargo run -p twcore -- init          # 生成一份带注释的 config.yaml
cargo run -p twcore -- check         # 只校验，不启动
cargo run -p twcore -- serve         # 起网关和控制面
```

## 它做什么

把客户端（Claude Code、Codex 之类）指向本地的一个端口，然后：

- **按规则路由**到不同上游 —— 条件可以是模型名、客户端、上下文长度、
  有没有工具调用等等，动作是换上游、改参数、或者直接拒绝
- **故障转移**：首字节之前可以透明换一家，之后只能如实报错
- **看得见成本**：token 用量、缓存命中、按价目表计价，算不出价钱的
  明确标「未知」而不是编一个数字
- **出站脱敏**：发给中转站之前把请求里的密钥换成占位符，模型回显时
  再换回来
- **入站审查**：上游返回的工具调用过一遍规则，高危的可以在那一帧上切断

## crate 分层

```
tw-types · tw-protocol · tw-provider · tw-resil · tw-crypto   ← 外部现实决定形状
tw-engine · tw-pricing · tw-redact · tw-yaml · tw-secret      ← 领域逻辑
tw-config · tw-store · tw-scan · tw-adopt · tw-observe        ← 装配
tw-gateway · tw-control                                       ← 数据面 / 控制面
```

上面两层对外部稳定，服务端版本直接依赖它们；下面两层是单机的场景的实现
（SQLite、unix socket），不共用。

## 开发

```
cargo test --workspace     # 单元与集成测试
scripts/smoke.sh           # 从零起，在真二进制上把每条路走一遍
```

`scripts/smoke.sh` 不碰你自己的任何东西 —— `HOME` 和 `THINKWATCH_HOME`
都指向一个临时目录，跑完就删。

## License

MIT
