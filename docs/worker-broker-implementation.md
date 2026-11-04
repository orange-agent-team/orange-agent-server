# E08 本机资源仲裁内核

2026-11-04。新增核心工程 `crates/worker`（`orange-worker`），对应既定 E08，不新增仓库或第 12 个核心工程。当前为库与自动化测试，**没有启动 GPU 供给服务，没有把探针替身当作真实 GPU 清理证据**。

## 已实现的执行边界

Broker 使用私有 SQLite/WAL/FULL 和独占文件锁，存储固定设备/审核 profile 配置摘要、监督器身份、命令、grant、slot 与不可变审计事件。一个本地实例持有数据库；并发请求经过同一状态机，只有一个有效槽许可。配置变化被拒绝，不能静默丢弃旧任务后换一套配置启动。

首次启动及重启均进入 `reconciling`。重启递增 broker epoch 和 slot generation，撤销远程供给开关，将仍未清理的 grant 隔离。已有监督器句柄不能沿用；真实 OS 连接重新认证后才可登记。世代溢出明确拒绝，不回绕。

`Acquire` 校验已登记监督器、固定设备和审核 profile；持久保存 request ID、请求摘要及结果。同键不同内容或主体拒绝；同键旧许可失效时不能重新创建执行。预留 15 秒，`ConfirmStart` 是一次性启动屏障，已经确认过的许可不会再次给出启动许可。远程预留还必须有同 node/attempt/lease/connection epoch/fencing 的已验证云端 StartGrant，接受分配或 ACK 本身不能启动引擎。

已验证云许可按单调发送时间、授予时长减 1 秒余量计算停止期限，最多 45 秒。响应接收时间不会延长授权，未来发送时间被拒绝。本机监督器续租最多 15 秒，未提供新的有效云许可时不能跨过已有云停止期限。租约过期进入排空；它不是释放设备的证据。

本地请求可原子撤销未启动的远程预留并取得更高 generation；运行中的远程任务则进入 draining/local_pending，先停远程新分配，等待真实监督器排空。旧 StartGrant、Renew、Drain 或 Release 不能作用于新持有者。抢占后远程供给保持关闭，需重新评估用户贡献策略。

`Release` 与 `Reconcile` 调用独立可信 OS 探针。先持久化隔离，再发带 device、epoch、generation、旧 grant 和随机 nonce 的 challenge；最多等待 5 秒。全部受管任务结束、全部受管进程退出、设备探针可用且健康、challenge/旧 grant 一致，才能增加 generation 并变为 idle。缺失、超时、错误或调用 Future 被丢弃均保留隔离。成功观测正文及其证据摘要写入不可变事件，远端布尔 CleanupReport 不能直接转换成本机证明。

休眠恢复、设备重置、OS 身份变化入口会原子失效所有旧许可、停止供给并进入核对。真实平台事件监听器尚待接入，不能把内核的 `invalidate` 单元测试视为已经通过真实休眠或驱动重置。

## 接口的信任来源

`VerifiedPeer::from_verified_os_identity` 仅供 OS 鉴权适配器使用；PID 单独不构成身份，必须包含用户、进程启动身份及实例 nonce。`Supervisor` 凭证无公开字段、无序列化或 Debug 输出。当前没有对外暴露接收任意身份字符串的 IPC/HTTP 入口。

`VerifiedCloudGrant::from_verified_start` 仅供完成 mTLS、当前云端 lease 与 StartGrant 校验的客户端适配器使用；该构造器本身不是 mTLS 或证书校验实现。`ResourceProbe` 仅由可信本机 OS 适配器实现，不能把不可信 JSON 反序列化为“设备已清理”。可信适配器尚未完成时不能注册供给。

`set_remote_allowed` 和 `invalidate` 是可信宿主钩子；未来 IPC 必须分别绑定已登记用户和真实 OS 事件权限。Broker 不接受用户文件正文、任意 shell 命令或任意 PID 终止请求。

## 验证与待办

`python3 scripts/check-broker.py` 运行实际临时 SQLite/文件锁与并发测试，覆盖单例、唯一获准者、幂等内容冲突、ACK 与启动分离、预留/运行抢占、旧世代拒绝、重启、保守期限、缺探针隔离、探针 Future 中断、私有路径/链接防护、休眠失效入口和不可变审计。

测试明确使用 OS 身份、云授权、设备探针替身。当前真实证据只有本机 Rust/SQLite/文件系统行为；Windows Named Pipe/DACL、SID/进程创建时间核验、Job Object/WSL 监督、NVML、GPU 型号映射、真实驱动重置和清理仍未认证。纯 CPU 任务不需要这个 GPU Broker。

后续实现：Worker 注册/CSR/mTLS、云端持久命令和会话世代、Worker 命令日志、两阶段用户态监督器、平台安全 IPC、真实设备探针、贡献策略，以及恢复后的配置/profile 升级流程。Broker 库和目录存在不代表 E08 整个工程完成。
