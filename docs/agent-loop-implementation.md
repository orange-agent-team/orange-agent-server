# E03 本地受控 Agent 执行切片

日期：2026-11-04。状态：真实 SQLite/文件工具控制链路实现；模型响应由明确的测试替身提供。**没有把此结果认定为真实模型推理、完整 Web Agent、云 Host 或 G3 完成。**

## 代码与边界

- `crates/agent-runtime/src/engine.rs`：Run 配置、持久计数、单步推进、批准入口、响应与工具意图原子提交、取消与恢复。
- `storage.rs`、`schema_v2.sql`：私有路径、独占锁、SQLite schema 1→2 迁移、旧数据保留、外键检查和 Host epoch。
- `workspace.rs`：已有真实 `read_file.v1`、`list_directory.v1`，模型只能提出这两个工具的严格类型参数；均需当前用户批准。
- `crates/protocol/src/model.rs`：完整 assistant/tool 配对历史、唯一 call ID、未知危险字段拒绝和 256 KiB 请求上限。
- `crates/model-runtime/src/llama.rs`：内部 ToolCall 显式映射为 llama.cpp 原生 function/arguments 字符串，不能直接把内部 DTO 当模型协议。
- `examples/local-agent.rs`：可信本地开发 CLI，组合 E03 与 E04；仅允许 E04 的 loopback 模型端点，没有远程付费模型或自动批准选项。

这不是 HTTP 权限系统或 OS 进程沙箱。`owner` 来自可信调用层，不能直接接受未认证网页传入的身份。`ModelGateway` 由可信应用注入，模型不能调用 Journal 的批准方法。云 Host 必须继续接入真实会话、平台审批和 PostgreSQL RunStore，不能用本地 CLI 代替。

## 持久化与恢复

`start_agent` 固定 owner、Run ID、模型版本、工作区环境摘要、policy、初始消息、调用与步骤上限、截止时间；同 Run ID 修改配置返回冲突，不重置预算。最长 Run 期限一天，最多 50 次执行步骤、100 次模型调用，单次输出 1–4096 token；工具和模型派发均消耗步骤。

`advance_agent` 每次最多派发一个模型或一个文件工具。模型调用前提交 invocation、完整请求和摘要、epoch/fencing/attempt 及计数。响应必须完整、finish reason 与工具内容一致、所有工具参数均通过验证，才能在同一事务写响应正文、摘要及全部工具意图。任何一个未知工具或非法参数都会拒绝整个响应，不能只执行其中合法的一部分。

每项工具批准绑定操作 ID、参数摘要、工具版本、工作区、policy 和到期时间。工具先取得容量与授权租约，再写 dispatch 屏障，再访问文件。结果正文、模型视图和状态先持久化；随后独立事务将结果恰好一次加入上下文。两次事务之间重启会复用已提交结果，不再次派发工具。已知只读失败记录 `failed` 和稳定错误码；超时、中断、未知执行结果进入 `recovery_required`。

进程重启后 `dispatching` 操作和 `dispatched` 模型调用变为 `unknown`；同进程丢弃调用 Future 也不能自动重试。相同环境存在未解决 unknown 时，新建 Run 同样被拒绝。当前没有人工核对后清除 quarantine 的产品流程，不能手工改库宣称恢复成功。

每次再次提交模型前，通过受限文件工具重新验证已使用来源的摘要。文件删除、内容变化、目录快照变化均停止继续提交历史工具内容。重新验证是明确的只读来源校验，不改变原工具执行记录。它不是文件系统事务快照；检查之后外部编辑者仍可能修改文件，模型看到的是最后验证时的授权内容。已撤销 Run 的视图不再返回最终正文，恢复不会重新激活 Run。

取消先撤销授权，再持久化 inactive/cancelled/unknown 状态。文件线程可能仍在排空；不能把返回取消当作 OS 资源已经释放，调用层需要 `wait_drained`。当前取消/恢复不具备远程 Worker 或 GPU 回收认证。

## 存储升级

Journal 必须位于可信应用私有目录，Unix 目录要求 0700、文件要求 0600；拒绝数据库、锁及 sidecar 的符号链接/硬链接和公开可读文件。不会擅自修改已有用户文件权限。旧版本留下的非私有目录，需要由宿主迁移到私有应用目录后再打开。

schema 2 通过建新 operations 表、复制、删除旧表、重命名完成 CHECK 扩展。外键只在迁移事务外暂时关闭，提交前检查、随后重新开启。旧批准和 unknown 保留；旧响应没有正文时保持 NULL，不能伪造恢复消息。未来格式和非空未识别格式被拒绝，字节不修改。Windows 私有 ACL 与目标平台运行仍未实测，不能仅依据 Unix 结果认证。

## 本地入口

构建：`cargo build -p orange-agent-runtime --example local-agent --locked`。使用锁定 Rust 工具链；可执行文件为 `target/debug/examples/local-agent`。

CLI 配置 JSON 包含：`run`（RunConfig 全部字段）、`grant_id`、`workspace`、`journal`、`private_roots`、`model_endpoint`、`model_key_file`、`context_tokens`、`prompt`。配置及密钥文件为 0600，Journal 父目录为 0700；密钥目录、Journal 目录和工作区必须隔离。配置不包含明文模型密钥。

顺序：

1. `local-agent CONFIG grant-info` 显示工作区环境摘要，将其固定到 `run.environment`。
2. `local-agent CONFIG start` 持久化 Run；`show` 查看状态。
3. `local-agent CONFIG step` 请求本机模型或推进一项已批准工具。
4. `show` 检查 pending 请求，再显式执行 `approve OP_ID`；CLI 批准有效期最多 60 秒且不超 Run 截止时间。
5. `cancel` 撤销 Run 并等待本进程租约排空；Ctrl-C 同样触发取消。

CLI 的 `step` 需要实际可用、模型版本匹配的 llama.cpp 服务。目前固定官方 GGUF 尚未下载成功，不能宣称该真实模型闭环已经运行。`start/show/approve/cancel` 的接口已编译，模型无替代下载地址或静默降级。

## 验证

`python3 scripts/check-agent-loop.py` 运行真实临时 SQLite/文件系统测试、协议历史负例、原生请求映射，并构建 CLI。覆盖批准缺失/过期/越权、原子响应拒绝、明确工具失败、多工具顺序、重启预算、模型 Future 中断、取消、文件改变/删除、结果提交后的恢复、撤销后正文不返回、schema 迁移及未知格式拒绝。

全工作区 `python3 scripts/check.py` 同时运行格式、Rust 测试和 Clippy。测试模型为源码中的 `ScriptedModel`/`NeverModel`，仅证明控制语义；真实模型、真实桌面 UI、云 RunStore、外部工具副作用、流式用户体验、记忆/Skills/MCP 和更多模型协议仍需后续实现或独立认证。
