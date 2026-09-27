> [English](../model-catalog.md) | 简体中文

# 模型目录与解析（`model`）

`model` 由三部分组成：

- **`model::cache`**：`ModelInfoCache`，Kiro 模型目录在内存中的快照，带 TTL。
- **`model::resolver`**：`ModelResolver`，把客户端发来的任意模型名转换为一个具体的 Kiro 模型 id。
- **`model::reasoning`**：`ReasoningCapability`，根据模型的目录 schema 推导出该模型原生的 thinking/reasoning 支持情况。

## `ModelInfoCache`

一个线程安全（`Arc<RwLock<_>>`）、克隆开销很低的缓存，以模型 id 为键，通过两种方式填充：

1. **Fallback**（`load_fallback`）：`config::fallback_models()` 中的内置快照。在启动时、任何一次在线拉取成功之前使用；之后若某次在线拉取失败，也会再次使用（缓存会继续以过期数据保持可读，直到显式重新加载 fallback，而不会因为一次临时的拉取失败就变空）。
2. **在线**（`update`）：`crate::server::AppState::initialize` 会启动一个后台任务，定期调用 `model::resolver::fetch_available_models`（向 Kiro control-plane 主机发起 `ListAvailableModels` 请求），然后调用 `update`。这是一次**完全替换**而不是合并：新数据中不存在的模型 id 此后将不再被视为有效。

`is_stale` 报告缓存是否已超过 `MODEL_CACHE_TTL`（默认 1 小时），服务端自身的后台刷新循环用它来决定何时重新拉取。对于上限未知或无效的模型，`get_max_input_tokens` 会回退为 `DEFAULT_MAX_INPUT_TOKENS`（200,000）。

## `ModelResolver::resolve`

主入口，每个 API 路由处理器在处理每个请求时都会调用它。按以下顺序生效：

1. **别名表**（`config.model_aliases`）：精确匹配，把对外暴露的名称映射为内部 id（例如 `auto-kiro` -> `auto`）。
2. **`normalize_model_name`**：将客户端使用的各种拼写/版本命名约定规范化（见下文）。
3. **在线目录检查**（`ModelInfoCache::is_valid_model`）：若规范化后的名称与缓存中的某个模型匹配，即以此为结果（`source: "cache"`，`is_verified: true`）。
4. **隐藏模型映射**：由 resolver 配置的映射，用于那些仍可解析但不在 `/v1/models` 中公开的模型（`source: "hidden"`）。
5. **透传兜底**：若以上都未匹配，则不经验证直接使用规范化后的名称（`source: "passthrough"`，`is_verified: false`），而不是直接拒绝请求。这样即使请求的是 Lanius 尚未收录的模型，也仍有机会直接在 Kiro 端成功。

## 模型名规范化

`normalize_model_name` 把 Claude 模型名规范化为一种稳定形式（`claude-<family>-<major>.<minor>`，例如 `claude-sonnet-4.5`），吸收 Anthropic/Kiro 客户端历来对同一模型使用过的各种拼写约定：

| 约定 | 示例 | 规范形式 |
|---|---|---|
| 标准，次版本号用短横线分隔 | `claude-sonnet-4-5-20250929` | `claude-sonnet-4.5` |
| 标准，无次版本号 | `claude-sonnet-4-20250514` | `claude-sonnet-4` |
| 旧式，family 在末尾 | `claude-3-7-sonnet` | `claude-3.7-sonnet` |
| 点号分隔并带末尾日期 | `claude-haiku-4.5-20251001` | `claude-haiku-4.5` |
| 倒序并带末尾修饰符 | `claude-4.5-opus-high` | `claude-opus-4.5` |

匹配在小写副本上进行；如果名称本身已是规范形式，或不匹配任何已知模式（例如 `gpt-4`、某个别名），则按其*原始*大小写返回，使无法识别的名称能逐字节原样往返。若存在末尾的 `[<n><unit>]` 上下文大小后缀，总会先通过 `strip_context_suffix` 去掉。

## 公开的模型列表

`get_available_models` 取在线目录的 id、隐藏模型映射的 id 以及所有已配置别名名称的并集，再减去 `hidden_from_list` 中的条目，然后排序。别名名称总会显示，即便其解析目标是隐藏的，因为别名才是客户端应当使用的名称。`get_available_model_details` 在此基础上补充每个模型缓存中的 `description`/`rateMultiplier`（对于没有对应缓存行的纯别名条目为 `None`）以及 `supports_thinking`。

## 原生 reasoning/thinking（`model::reasoning`）

Kiro 的 `ListAvailableModels` 响应会通过 `additionalModelRequestFieldsSchema` 描述每个模型接受哪些额外的请求字段。有两类模型在这里暴露了原生 reasoning：

- **Claude 风格**（`ReasoningProtocol::Thinking`）：一个 `thinking` 对象（`type`：`adaptive`/`disabled`，`display`：`summarized`/`omitted`），外加可选的 `output_config.effort` 级别。
- **GPT 风格**（`ReasoningProtocol::Reasoning`）：只有一个 `reasoning.effort` 级别。

两种字段都没有的模型不会得到任何 reasoning 字段，也完全没有 thinking 输出。`ReasoningCapability::from_model` 把 schema 解析为类型化的能力描述（允许的 effort 级别、thinking 类型、display 模式、默认 effort）；随后 `ReasoningCapability::request_fields` 把客户端的 `ReasoningRequest` 转换为发送给 Kiro 的确切 `additionalModelRequestFields` 值，并将请求的 effort 级别吸附到模型实际支持的*最接近*级别（`nearest_effort`），而不是直接拒绝不支持的级别。

`EffortLevel` 是一个单一的有序枚举，同时覆盖 OpenAI 的词汇（`none`..`xhigh`）和 Kiro 的词汇（`low`..`max`），因此无论通过哪种客户端协议请求的 effort，都会先映射到同一个刻度上，再吸附到目标模型支持的级别。

`returns_visible_thinking` 决定模型的 reasoning 是否真正以可见文本的形式呈现给客户端（Claude 风格模型会；GPT 风格模型接受 reasoning effort，但 Kiro 从不返回它们的 reasoning 文本，所以即便它们在内部确实会"推理"，这里也总是报告 `false`）。`ModelDetails::supports_thinking` 和 `OpenAIModel::supports_thinking` 向客户端报告的就是这个值。
