# 受管 CPU Worker 单任务执行

后继更新：已增加可选、独立协商的 [请求绑定云续期](worker-execution-renewal.md) 及 SQLite schema4。以下记录初始 CPU 所有者的边界；新客户端通过 `connect_renewable_execution` 开启续期，旧客户端仍受原租约限制。当前真实模型验收增加至5项，包含保持实际进程48秒后跨原期限生成；输出和结算仍未实现。

本切片连接真实用户输入对象、mTLS Assignment、Broker 启动许可和现有 E04 `ManagedCpuModel`。入口为 Unix 平台的 `orange_worker::cpu::spawn_cpu(CpuResources, CpuExecutionSpec)`；仍不是常驻安装服务或完整 Job 交付链。

## 所有权与执行

`CpuResources` 独占控制连接、节点日志、执行日志、Broker 和 Supervisor。调用方不能在模型执行中同时修改它们；`wait()` 成功后才取回资源。取消 `wait()` 的 Future 不消费结果接收器；取消或丢弃公开 Handle 都向所有者发送停止信号，不中止负责回收的任务。

按序检查审核 profile、完整模型摘要、原始请求投影、当前 Assignment 和私有执行目录，然后执行 reserve、ACK/StartGrant、ConfirmStart、租约绑定输入读取。本机 SQLite schema3 在启动进程前写入唯一 command/instance/request/profile 记录；原执行不重放。运行目录绑定规范路径、设备号和 inode，不能通过换一个空目录绕开未知进程。

模型任务通过现有 CPU 监督器校验可执行文件和权重，启动真正的 llama-server，以私有 socket/API key 调用一次模型，再通过拥有的 Child 句柄停止并等待退出。公共 Worker 不运行 Agent 工具。执行 profile 摘要包含后端与模型 SHA、大小、模型版本、上下文、启动上限和固定 CPU 参数。

独立所有者在生成期间每秒续本机 Broker 监督，每两秒执行节点心跳及当前输入授权检查。网络等待不占用 Broker 事务，也不阻止本机期限检查。节点命令、输入删除、权限丢失、网络失败或监督超时都会取消模型并等待退出。候选结果必须在模型结束后经过新一轮当前授权检查才能返回。

本机 `renew(None)` **不能延长云执行租约**。现有云端授权窗口到期即停止；长任务需要后续正式续期协议。当前收到节点命令会停止本次模型，但正式节点策略应用及动作完成仍通过既有 NodePolicy 接口处理，尚未整合成常驻控制器。

## 回收与结果

所有者首先进入 draining，等待模型任务结束，再用匹配实例的 stopped 收据、任务 join 和实际空闲 `engine.lease` 锁构成私有 CPU 探针，交给 Broker 校验 grant/epoch/generation/challenge 后释放逻辑 CPU 槽。没有明确退出证据就保持占用/隔离，返回 `cleanup_unknown`，不返回候选结果。这里没有按裸 PID 恢复或杀进程。

`CpuRunReport` 包含 outcome、候选正文、授权检查/监督次数、清理确认及有限错误码。本机日志只保存请求/候选/清理摘要，不保存候选正文。成功生成的 outcome 为 `computed_unsubmitted`，`cloud_completion` 为 `pending_output_commit`。没有输出对象、ResponseCommit、DR-01 实际使用量或账本提交时，云 Job 不转成功、不扣费也不伪造退款。

所有者重开时将未结束 cpu_runs 改为 `cleanup_unknown/OWNER_RESTARTED`；不会重放模型或将空目录视为退出。进程启动失败、启动前许可之后的部分错误和宿主崩溃仍可能需要保守人工/OS 所有权恢复；本切片未提供任意孤儿自动回收。直接所有者进程被强杀的父生命周期保障、Windows/Linux 目标认证、GPU 回收和后台服务安装尚未交付。

## 可重复验证

运行 `python3 scripts/check-real-worker.py`。脚本要求现有固定完整 Qwen 权重和 CPU-only llama.cpp 构建，验证两个文件的 SHA、模型字节数和 backend commit；缺失则失败。真实本地 PostgreSQL、mTLS、对象文件和模型进程覆盖：

- 实际生成候选、当前授权再检查、清理后 CPU 空闲、禁止重复启动，云结算仍 pending。
- 生成中显式取消，候选不暴露且真实进程退出。
- 生成中删除源对象，持续授权失败后取消并回收。
- 等待 Future 被取消及公开 Handle 丢弃，所有者仍完成实际停止并释放继承锁。

`python3 scripts/check-broker.py` 包含 schema1 升级、CPU 不可重复启动、目录绑定及重开 unknown 测试。普通 `check-worker-session.py` 继续验证原控制接口。完整 Rust 检查使用 `python3 scripts/check.py`，集成特性另执行 Clippy。

报价、目录、额度、证书注册、初始 OS 身份/设备准入仍是明确夹具；测试只认证对应真实本地路径。准确通过结果和源码摘要在统一执行台账，不将这些夹具或本地文本当生产能力、云端已交付结果或收费认证。
