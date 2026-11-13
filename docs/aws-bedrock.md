# AWS Bedrock 接入

核对日期：2026-11-13。实现使用官方 AWS SDK for Rust 的 `InvokeModel` / `InvokeModelWithResponseStream`，目标是在 EC2 Instance Role 下调用 Bedrock。已通过用户提供的隔离 `8081` 实例完成四款模型的真实普通/流式请求、兼容接口、工具及签名历史回传验证；真实 Instance Profile 凭据来源、过期刷新、长上下文和限流/配额边界仍未完成主机级验收。

## 入口与模型

示例显式设置 `region = "us-east-1"`，SDK 调用 `https://bedrock-runtime.us-east-1.amazonaws.com`。Global 是模型推理配置 ID 的 `global.` 前缀，不是单独的无区域 HTTP 域名；请求可能在全球支持的商业区域处理。其他源区域需要另行核对每款模型的可用性。

| 对外模型名 | Bedrock Global 推理配置 ID | 官方模型页 |
| --- | --- | --- |
| `claude-opus-5-5` | `global.anthropic.claude-opus-5-5` | [Opus 5.5](https://docs.aws.amazon.com/bedrock/latest/userguide/model-card-anthropic-claude-opus-5-5.html) |
| `claude-sonnet-5-5` | `global.anthropic.claude-sonnet-5-5` | [Sonnet 5.5](https://docs.aws.amazon.com/bedrock/latest/userguide/model-card-anthropic-claude-sonnet-5-5.html) |
| `claude-haiku-5-5` | `global.anthropic.claude-haiku-5-5` | [Haiku 5.5](https://docs.aws.amazon.com/bedrock/latest/userguide/model-card-anthropic-claude-haiku-5-5.html) |
| `claude-fable-5-2` | `global.anthropic.claude-fable-5-2` | [Fable 5.2](https://docs.aws.amazon.com/bedrock/latest/userguide/model-card-anthropic-claude-fable-5-2.html) |

这四个 ID 为当前适配器的明确目录。上游模型的 `protocols` 省略，由适配器确定 Anthropic Messages 原生协议；公开模型仍可启用既有 Chat Completions 和 Responses 转换。优先使用 `/v1/messages` 可保留 Claude 原生 thinking、工具及未知扩展字段。

示例和三文件包配置同时接受短名和对应的 Global ID，例如 `claude-haiku-5-5` 与
`global.anthropic.claude-haiku-5-5`。实际 AWS 请求始终使用路由目标中的 Global ID。
对外模型名由 QuotaMux 配置决定，与后台的 Bedrock 模型 ID 分开。客户端使用
`claude-haiku-5-5`、后台映射到 `global.anthropic.claude-haiku-5-5` 是正确配置；
Global 名称别名只是可选兼容，不是 Bedrock 接入要求。
`CLAUDE_CODE_USE_BEDROCK`、`ANTHROPIC_MODEL` 等 Claude Code 环境变量不配置 QuotaMux；
QuotaMux 由 `quotamux.toml` 决定路由。

### 收到 no route target is currently available

这条 503 是本次没有可调用目标的网关响应，不能仅凭它判定 AWS 根因。
使用更新后的三文件包，一次运行完整诊断：

```sh
./start.sh --diagnose
```

结束后只需回传终端显示的 `logs/diagnostics-时间-PID.json`。报告包括配置与环境变量存在性、
凭据解析和 STS 身份、正在运行的本地网关的原始错误/熔断快照，以及四款模型各一次普通、
一次流式调用。探测使用固定短提示、低 effort 和最多 256 个输出 token，可能产生少量模型用量。
每次模型探测最多 45 秒，八项顺序执行，最坏约七分钟；某项失败仍继续能独立执行的项目。
凭据加载失败时记录原因并跳过模型；STS 失败不会阻止已有凭据的模型调用。

诊断不启动网关、不打开业务数据库，不重置现有熔断；可在原网关仍运行时执行。
报告目录权限为 `0700`、文件为 `0600`，不覆盖旧文件；不记录请求/响应正文、thinking、
签名或 HTTP 头，已知凭据值会脱敏。仅回传诊断 JSON，无需逐条运行 curl 或回传整份 TRACE 日志。
退出码 `0` 表示全部通过，`2` 表示诊断发现问题且报告已保存，`1` 表示参数或报告写入失败。
自定义位置可用 `./start.sh --diagnose --report /私有目录/report.json`，目录必须为真实的私有目录。

报告中的 `targets[].circuit.reason` 和 `next_probe_at_ms` 给出暂停原因与下次探测时间；
尝试记录保留首次调用的上游状态和错误类型。现有鉴权/配置失败首次冷却为 300 秒，
期间的后续请求可能只看到上述 503。2026-11-13 的模拟测试已复现该路径，
2026-11-13 的远端只读记录显示，历史失败为 Role 缺少 `bedrock:InvokeModel` 的
`AccessDeniedException`；随后四模型的真实普通/流式请求已全部成功。原始 503 的具体请求号
未在当前账本检索到，因此历史权限拒绝与该次 503 的关联仍是结合时间和熔断机制的判断。
名称兼容配置不会重置熔断状态。实测实例以短名对外、Global ID 调用上游，符合 QuotaMux 的使用方式，
无需增加别名。仅当希望客户端直接使用 Global ID 作为对外模型名时，才需要配置相应 aliases。

## 配置与凭据

完整独立示例在 [quotamux.bedrock.example.toml](../quotamux.bedrock.example.toml)，包含四款模型，使用 `8081` 与独立数据目录。可以将其中的 backend、公开模型和路由层合入自己的配置，但不要覆盖现有生产配置。

```toml
[[backends]]
id = "aws-bedrock"
adapter = "aws-bedrock"
region = "us-east-1"

[[backends.credentials]]
id = "instance-role"

[[backends.models]]
name = "global.anthropic.claude-opus-5-5"
```

`instance-role` 是网关的路由标识，不是 Role ARN，也不会执行额外的 AssumeRole。一个 Bedrock backend 只允许一个身份条目，可通过既有 `pool` 关联本地并发限制。`api_key` 必须省略或为空，`endpoint` 不允许配置。区域显式配置并参与路由身份摘要，变更区域会重建客户端和相应熔断状态；已有 API Key 供应商的身份摘要保持兼容。

正式客户端使用 AWS SDK 默认凭据提供链与身份缓存，延迟到首次调用时解析凭据，由 SDK 刷新临时凭据。EC2 上绑定 Instance Profile 后，SDK 可通过 IMDSv2 获取 Role 凭据。环境变量、共享配置等凭据源在默认链中优先于 EC2 Role，因此部署时要核查是否有不需要的 `AWS_ACCESS_KEY_ID`、`AWS_SECRET_ACCESS_KEY`、`AWS_SESSION_TOKEN` 或 AWS profile 配置。网关只使用该链提供的签名凭据，调用端不会继承 Bedrock bearer token 或环境端点替换。[AWS Rust 凭据链](https://docs.aws.amazon.com/sdk-for-rust/latest/dg/credproviders.html)

账户界面可编辑区域，并显示无需 API Key 的说明。工作区保存仍对新建或变化的绑定执行真实上游校验：没有 AWS 环境时应编辑、检查配置草稿，不能把保存成功当作模拟验收。`--check` 只检查配置结构，不会获取凭据或调用模型。

## EC2 Role 权限

[aws-bedrock-ec2-policy.example.json](aws-bedrock-ec2-policy.example.json) 按 AWS 的三段策略组织，包含四款模型的普通与流式调用权限。将所有 `ACCOUNT_ID` 替换为调用账户的 12 位 ID，再由部署人员附加到 EC2 Role。示例不会自动上传、创建或更改 AWS 资源。

三段权限分别限定源区域的 global inference profile、源区域 foundation model、区域无关 foundation model。最后一段使用 `aws:RequestedRegion = "unspecified"`，模型授权还限定 `bedrock:InferenceProfileArn`。如果组织有 Region 限制的 SCP，也需要按组织策略允许该 global 授权路径；只放行 `us-east-1` 可能仍然被拒绝。Role 的信任策略/Instance Profile 绑定、模型访问资格和配额也需要在真实账户确认。[AWS Global 推理 IAM 与 SCP 文档](https://docs.aws.amazon.com/bedrock/latest/userguide/global-cross-region-inference.html)

EC2 容器部署还要确认容器能够访问 IMDSv2。AWS 文档指出，hop limit 为 1 时容器可能收不到响应，并讨论了 hop limit 为 2 的部署方式。这里仅记录条件；本次没有更改任何机器的元数据、网络或系统设置。[EC2 IMDSv2 数据检索](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/instancedata-data-retrieval.html)

## 协议与故障处理

请求只作 Bedrock 所需的封装变化：模型 ID 放入 API 路径，移除 JSON 的 `model` / `stream`，设置 `anthropic_version = "bedrock-2023-05-31"`，将 `anthropic-beta` 请求头合并到 `anthropic_beta` 数组。消息、thinking 签名、工具、缓存检查点和其他语义字段保留，由上游判断合法性。[AWS Messages 请求参数](https://docs.aws.amazon.com/bedrock/latest/userguide/model-parameters-anthropic-claude-messages-request-response.html)

普通响应保留原生 JSON；流响应由 SDK 解码 AWS EventStream，再封装为 Anthropic SSE，保留事件数据。SDK 内部重试关闭，路由层管理回退。`ValidationException` 属于请求错误，不回退、不影响熔断；鉴权、模型配置、限流/配额与服务暂时故障分别进入已有错误分类。HTTP 状态及 `Retry-After` 用于现有诊断和冷却流程。首个语义事件前允许回退，提交后流异常会终止并记录失败，不会拼接另一个上游的回答。[InvokeModel](https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_InvokeModel.html)、[流式调用](https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_InvokeModelWithResponseStream.html)、[AWS 流异常](https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_ResponseStream.html)

`server.timeouts` 的连接与总期限同时应用于 AWS SDK，普通响应受 `upstream_read_ms` 操作预算约束；流首响应及后续每次接收受 `upstream_stream_read_ms` 约束，总期限贯穿整条流。客户端取消时丢弃该请求的 SDK 接收流。

这几款模型的保留 thinking 与模型及原始对话前缀绑定，部分还与 AWS 账户绑定。原生请求不重写这些字段；自行配置回退时应注意跨模型或跨账户重放可能被上游拒绝。接入没有修改模型的默认 thinking 或采样语义。[Sonnet 5.5 模型约束](https://docs.aws.amazon.com/bedrock/latest/userguide/model-card-anthropic-claude-sonnet-5-5.html)

上游提供 `usage.output_tokens_details.thinking_tokens` 时，统计采用真实计数（包括显式 `0`），
普通和流式请求均不再以字节估算覆盖；Chat Completions 和 Responses 转换保留对应的 reasoning
明细。历史记录不会重新计算。只有上游未提供有效明细时，才沿用既有思考 token 估算。
[AWS 思考 token 用量](https://docs.aws.amazon.com/bedrock/latest/userguide/claude-messages-adaptive-thinking.html)

`/v1/messages/count_tokens` 继续提供带 `x_quotamux_estimated: true` 的网关本地估算，不调用 Bedrock `CountTokens`，不能当作 AWS 精确计数。示例未填价格，避免把未核实的费率记成实际账单。

## 验证与后续验收

### 三文件 Linux 静态包

ARM64 和 x86_64 分别打包，每个 ZIP 严格只有 `quotamux`、`quotamux.toml`、`start.sh`。
在对应架构的 EC2 上解压到独立目录后执行 `./start.sh`；程序前台运行，默认地址
`http://127.0.0.1:8081`，按 Ctrl+C 停止。数据保存在解压目录的 `data/`，
TRACE 最大日志同时输出到终端和私有 `logs/quotamux-时间-PID.log`。
`./start.sh --check` 可只做配置检查；`./start.sh --diagnose` 一次生成完整诊断报告。

专用构建启用 `standalone-tls`，内置经过校验的公共根证书，使用 musl 静态链接；
EC2 不需要安装 Rust、Docker、动态运行库或独立 CA 包。默认开发构建保留系统证书行为。
包中不包含密钥，真实推理仍要求 EC2 Role、模型授权和 global 配额就绪。

两个最终 ZIP 均通过 ELF 架构、动态加载器/共享库缺失、文件数量/权限/CRC 检查，
并在各自架构的 Linux 容器中验证了配置、健康接口、四模型目录、真实启动脚本、TRACE 和 Ctrl+C。
无 CA 文件的 scratch 容器使用假凭据收到 AWS 403，证明内置 TLS 校验有效；没有进行模型推理。
构建、三文件打包和验证脚本位于 `packaging/ec2/`，产物与校验和位于忽略目录 `data/releases/`。

### 自动化检查

本地测试使用 loopback 假服务、固定假凭据以及带 CRC 的 AWS EventStream 帧，检查真实 SDK 发出的 SigV4 区域/服务签名、四款模型路径、请求保真、流式 thinking/工具/usage、异常分类与提交边界。测试端点替换只在 `test-support` 中启用，正式配置不能使用。

```sh
cargo test --locked --all-targets --all-features -- --test-threads=1
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo run --locked --bin quotamux -- --config quotamux.bedrock.example.toml --check
```

当前远端已验证四款模型的普通/流式请求、Haiku 的两种 OpenAI 兼容接口、Haiku/Fable 工具往返及
Fable 的流式 thinking 签名历史回传。实例当前的短名到 Global ID 映射正确，可以直接调用；
思考 token 记账遗漏已修复并补回归，代码/静态包修复不代表该实例已经更新。
后续仍需在独立 `8081` 与独立数据中核实 Role 身份来源、真实临时凭据刷新、长上下文和限流表现。
只有完成真实环境验收后，才按项目流程考虑生产 `8080`。本次没有合并或修改生产实例。
