# 开发 Worker 进程与节点停止接线

后继更新：[输出交付切片](worker-output-delivery.md)已接真实对象/ResponseCommit和仅查询恢复。新配置commit_outputs默认true并要求有writer的Gateway；显式false才保留原未提交联调模式。User API可配置双入口共享writer，结算、云端cleanup、正式安装仍未完成。以下保留原Host切片的初始范围与诊断事件说明。

本切片提供可实际运行的 `orange-worker` Unix 开发进程，连接已有 mTLS Gateway、持久 Journal/Broker 和真实 CPU 模型。它没有安装系统后台服务，不支持公开生产入口或 Windows/GPU，也尚未实现输出交付与结算。E08仍在原服务端仓库，不新增核心工程或仓库。

## 启动配置

构建：`cargo build -p orange-worker --bin orange-worker --locked`。运行：`target/debug/orange-worker /绝对路径/私有配置.json`。服务端沿用已验证Rust1.97.1；没有隐式远端发布或付费模型调用。

参考 `config/worker-development.example.json`。模板含占位符且贡献开关为false，不能直接运行。配置必须明确给出已预配置的节点身份、审核profile及准确模型/可执行文件摘要。真实enrollment、模型目录和价格配置API还需后继交付；验收中这些云端记录由隔离夹具建立，不伪装成用户已完成注册。

| 配置 | 要求与用途 |
| --- | --- |
| protocol / environment | 固定1 / development；未知字段、未来协议和production拒绝 |
| state_directory | 已存在、当前有效UID拥有、绝对路径、0700目录；保存私有Broker/Node/Execution数据库 |
| endpoint / server_name | HTTPS loopback端点及匹配TLS服务名；不允许远端公开地址、query或userinfo |
| server_ca / certificate / private_key | 预配置mTLS文件的绝对路径；父目录私有、当前UID、文件0600、单硬链接、拒绝symlink，每个最大64KiB |
| node_id / profile_id | 已登记节点与审核profile标识；云Assignment必须准确匹配本机profile和摘要 |
| allow_remote_contribution | 必须显式true；不能因启动进程推断用户同意贡献算力 |
| profile | executable/model绝对路径和SHA256、模型字节数、model_version、context_tokens512—32768、startup_timeout_seconds1—120 |

配置文件本身同样要求私有父目录/当前UID/0600/单链接，最大32KiB。凭证不写入源码或日志。实际模型/后端在已有监督器内再次完整校验；配置通过不代表文件安装或模型审核已通过。

## 本机身份与准入

进程内Supervisor的owner从 `rustix::process::geteuid()` 获取，不接受JSON中的owner。使用系统随机数生成新的进程/实例nonce，并与Broker epoch绑定；PID只作为当前进程标记的一部分，不按裸PID恢复或杀进程。原仓库的 `unsafe_code=forbid` 保持开启，rustix固定为已有锁文件版本1.1.6。

state_directory/model是实际受管模型目录，先通过ExecutionJournal绑定规范路径/dev/inode。启动准入检查真实文件权限、engine.lease排他锁和process.json；只允许没有未决旧grant的空目录，或已有stopped记录且锁真实空闲的状态。旧grant、ready/unknown记录或活跃锁都会拒绝自动接管，不把“没看见PID”当退出证据。

该探针只证明这个私有受管CPU槽的准入，不证明整机CPU独占、GPU空闲或其他OS设备健康。此进程内身份也不是Named Pipe/Unix socket外部客户端认证。桌面与供应方共享Broker IPC、OS服务安装和跨进程恢复仍需后续接线。

## 执行、停止与诊断接口

控制循环持续接收持久Assignment、节点命令和心跳；模型运行委托给既有CPU所有者，独立维护本机监督和云续期。SIGTERM/SIGINT会请求取消并等待模型实际退出后结束。模型任务不会因公开Handle丢弃而被直接abort。

心跳可能先报告新策略世代、具体命令随后到达。执行所有者此时先关闭贡献并取消模型，再在最多6秒内等待当前mTLS会话的准确命令回执。只有当前receipt、世代和新鲜授权都匹配才应用Broker停止策略；超时或丢弃授权Future使连接失效，报告POLICY_AUTHORITY_UNCONFIRMED。未确认命令不会生成策略应用记录，已停模型也不会恢复。

drain/quarantine成功应用后，Broker平台停止标记持续保留，普通贡献开关无法绕过。实际模型退出的证明仍来自owned Child等待、stopped收据和继承锁释放；本机CPU槽可以释放，而平台停止仍有效。任何本地结果都不直接把Admin动作改成succeeded，也不结算Job。

stdout使用版本1无正文诊断事件，schema为 `contracts/json-schema/worker-host-event.schema.json`。字段是protocol、state、command_id、outcome、cleanup_confirmed、policy_applied；不输出prompt、response正文或TLS材料。

- ready：控制会话已建立，不是执行授权或生产就绪证明。
- execution_stopped：本地执行结果及清理状态，可能为computed_unsubmitted/cancelled/authority_lost等。
- policy_applied：本机Broker已记录当前停止策略，不代表云Admin动作完成。
- pending_output_commit：候选仅保留在该进程内存中，尚未交付；不会继续启动第二个模型。
- unsubmitted_output_discarded：正常关闭时丢弃尚未提交候选；不自动重放原Job。
- stopped：控制连接关闭且执行所有者已结束。

诊断队列有界且允许丢弃事件，不能阻塞资源监督。持久Journal/Broker才是本地事实来源；stdout不是可靠事件总线。日志消费者停止读取时，模型仍能清理，Runtime退出不无限等待诊断线程。

## 验证及后续

`python3 scripts/check-worker-host.py` 构建实际二进制，读取Cargo真实target目录，再运行实际进程、真实UID/文件锁准入、固定完整模型、mTLS/PostgreSQL、受控SIGTERM和无正文事件测试。只对当前仍由测试独占持有且未回收的直接Child发信号，不扫描或恢复PID。初始证书注册、审核目录、额度及价格仍是明确夹具。

`python3 scripts/check-real-worker.py` 单独验证CPU所有者：包括真实模型续期、取消、删除、Handle丢弃、drain/quarantine以及缺命令的失败关闭。`check-broker.py`覆盖真实锁/未知记录/旧grant拒绝、私有配置与贡献同意等本机负例。

当前没有自动重连恢复、未知进程自动清理、后台安装、输出对象/ResponseCommit/DR-01结算或生产认证。候选在内存中的保留不能充当可靠输出存储；下一步必须实现租约范围内的输出上传、当前用户权限复核、精确版本提交及幂等结算，再开放真正的端到端Job完成流程。
