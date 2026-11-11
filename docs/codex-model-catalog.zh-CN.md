# Codex 远程模型发现

[English](codex-model-catalog.md)

Core 在 `GET /codex/models?client_version=<major.minor.patch>` 提供 Codex 原生模型目录，
复用推理请求的网关密钥、来源地址限制、模型目录、别名和密钥 `allow` 范围。
标准的 `/v1/models` 保持 OpenAI 响应格式。

ThinkWatch Lite 接管 Codex 时会配置这个地址。手动连接的例子：

```toml
model_provider = "thinkwatch"

[features]
api_key_model_discovery = true

[model_providers.thinkwatch]
name = "ThinkWatch"
base_url = "http://127.0.0.1:8788/v1"
model_catalog_url = "http://127.0.0.1:8788/codex/models"
wire_api = "responses"
env_key = "THINKWATCH_GATEWAY_KEY"
requires_openai_auth = false
supports_websockets = false
```

地址改成网关的实际地址，环境变量 `THINKWATCH_GATEWAY_KEY` 设为已启用的网关密钥。
移除生效的 `model_catalog_json`：本地目录文件会覆盖远程发现。选中的 profile 也可能覆盖
provider、本地目录或发现开关。Lite 会处理默认选中的 profile，断开接管时恢复原字段。
显式选择其他 profile 或传入命令行覆盖项仍会改变 Codex 使用的 provider。

连接只需配置一次。此后，上游模型、别名、启用范围、手动模型规格和密钥权限的变更，
都会在下一次目录请求里体现。Lite 和控制 API 的修改走正常热重载，无需向 Codex 配置
写入模型列表。首次连接需要重启 Codex。Codex 自己也有模型缓存，已经打开的模型选择器
可能在客户端刷新或重启之后才显示新列表；此接口不会向客户端推送更新。

## 原生元数据和普通上游

Core 向 Responses、Chat Completions 和 ChatGPT 上游获取元数据时，传入请求者的
Codex `client_version`，使用该上游的凭据和代理，不转发入站网关密钥。ChatGPT 使用原生
模型地址，直连 OpenAI API 时使用其文档指定的原生 Codex 接口，其他上游使用各自的模型接口。
完整的原生 `models` 条目会保留指令、工具和
版本特有的未知字段。标准 OpenAI `data` 列表没有原生指令，改用通用编程描述，不把
OpenAI 内置模型的指令套到无关模型上。

只暴露共享网关目录中已经存在的模型，原生目录不会独立添加模型或授予推理权限。
别名作为对外的 slug，不设置迁移目标。多个路由候选的原生定义不一致时，使用通用描述，
对已知能力取交集。手动模型规格优先于原生值，原生值优先于价目表。未知上下文窗口不填，
图片输入要求所有候选都明确支持。

推理档位优先采用上游原生元数据。上游只返回普通模型列表或原生元数据不可用时，Core
还会使用已收录的 OpenAI、DeepSeek 模型的[有据可查的推理档位](../crates/tw-gateway/data/codex_reasoning.md)。
DeepSeek Flash/V4.1-Flash 和 V4-Pro 提供 `low`、`high`、`max`，默认 `high`。
这些内置定义只补推理档位，不改通用指令和工具，并遵守各自的最低 Codex 版本要求。
仅凭模型规格中的 `reasoning: true` 不会给未知模型编造档位。

别名从实际发给上游的模型名继承档位，支持名称上的厂商前缀和 ASCII 大小写差异。
给别名起一个已知模型的名字，不会让它获得那个模型的能力。多个路由候选的档位单独取
交集，不会因为指令或工具不同而全部丢失，默认值也必须在交集内。手动
`model_specs.reasoning: false` 或任一候选没有已知档位时，该别名不提供档位选项。
上游明确提供的原生档位列表（包括空列表）优先于内置定义。
有候选需要协议转换时，还会排除 Core 无法在转换中表达的档位。Responses 和 ChatGPT
直通可以保留 `ultra` 等更新的原生档位。

## 缓存和限制

只缓存上游原生元数据。缓存按上游、解析后的凭据及请求头、地址、客户端版本和代理配置
隔离，最多 32 项，同时最多四次元数据请求，获取期限为八秒。成功响应最多缓存五分钟，
扣除 `Age`，遵守 `no-cache` 和 `no-store`。获取失败时使用通用描述，30 秒后重试。
上游元数据过大或不可用时也使用通用描述。

即使带条件请求头，网关也会重新生成名称、权限和能力，并在返回 `304` 前鉴权。获取元数据
期间发生配置重载，会先丢弃旧配置的原生定义，再根据当前配置生成目录。响应使用私有缓存，
30 秒后重新验证，携带 ETag。推理接口独立检查当前密钥和模型权限，不依赖 Codex 的缓存。

最终目录限制为 Codex 要求的 1 MiB。超过时返回 `413`，不会静默漏掉部分模型；可缩小该
网关密钥的模型范围。缺失、重复或格式错误的 `client_version` 返回 `400`。

远程目录和通用描述已用 Codex CLI 0.166.0 验证。客户端配置和版本化格式参见
OpenAI 的[模型目录接口契约](https://developers.openai.com/codex/enterprise/roll-out-a-gateway#implement-the-catalog-endpoint)。
