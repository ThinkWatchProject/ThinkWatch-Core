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
版本特有的未知字段。标准 OpenAI `data` 列表使用通用编程描述，并将上游报告的能力
映射到其中，不采用普通条目里的指令或工具。

只暴露共享网关目录中已经存在的模型，原生目录不会独立添加模型或授予推理权限。
别名作为对外的 slug，不设置迁移目标。多个路由候选的原生定义不一致时，使用通用描述，
对已知能力取交集。手动模型规格优先于原生值，原生值优先于价目表。未知上下文窗口不填，
图片输入要求所有候选都明确支持。

普通模型列表读取 `data[].context_window`、`data[].input_modalities`、
`data[].effort.supported_levels` 和 `data[].effort.default_level`。例如 DeepSeek 的
[模型接口文档](https://api-docs.deepseek.com/api/list-models)已定义这些字段。档位列表转为
Codex 的 `supported_reasoning_levels`，报告的默认值在列表内时保留。可选字段缺失或
格式错误时不编造能力，也不按模型名查表或仅凭 `reasoning: true` 推导档位；中转站需要
透传这些元数据即可自动获取；没有透传时可在模型规格中手动声明。

别名继承实际路由目标报告的能力。推理档位独立于完整原生定义取交集，不会因为指令或
工具不同而丢掉共有档位，默认值必须在交集内。手动 `model_specs.reasoning: false`
移除档位选项。任一候选需要协议转换时，只保留 Core 能表达的档位；Responses 和
ChatGPT 直通可以保留 `ultra` 等更新的原生档位。

## 自动获取与手动覆盖

默认读取上游实际报告的能力。后台模型清单保留同样的规格信息，供 Lite 和标准模型发现使用；
Codex 的版本化目录优先使用对应客户端版本的元数据。没有报告的档位保持未知，
不根据模型名称猜测，也不从价目表中的推理开关生成档位。

上游缺少模型列表时，先在 `models` 中手动添加真实模型 ID；缺少或错误的推理档位则按
该上游、该模型填写覆盖值：

```yaml
providers:
  - name: relay
    base_url: https://relay.example.com/v1
    protocol: openai-chat
    models: [private-model]
    model_specs:
      private-model:
        reasoning_effort:
          supported_levels: [low, high, max]
          default_level: high
```

`reasoning_effort` 整组替换上游档位，非空列表自动表示支持推理，不必另写 `reasoning: true`。
不写这一项是自动；`supported_levels: []` 明确表示不可调。`default_level` 可省略，
Core 在最终可用集合中优先选择 `medium`，否则选择最低档；手写默认值必须属于所列档位。
档位不能重复，`reasoning: false` 不能与非空手动档位并存。恢复自动时移除整项覆盖，
上游元数据和其默认值重新生效。别名交集与协议转换限制同样作用于手动档位。

控制 API 的模型行分别提供上游报告、手动原值、保存后的生效结果及协议过滤掉的档位。
`PUT /provider-model-spec` 的 `reasoning_effort` 以 `mode: keep`（省略时亦如此）、
`mode: auto`、`mode: set` 加 `value` 区分保留、清除和整组替换；编辑其他规格不会误删档位。

## 缓存和限制

只缓存上游元数据。缓存按上游、解析后的凭据及请求头、地址、客户端版本和代理配置
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
