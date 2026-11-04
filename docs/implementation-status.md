# 实施状态与本机验证

日期：2026-11-04。这是G0/P1/P2/P3的局部实现，**不是完整G3或G6候选版**。

## 已实现

- orange-domain：整数金额、规范字符串、计价/费率、预算限额、取消/完成互斥、旧响应和GPU回收事实检查。
- orange-protocol：严格DTO、JSON金额无损传输、请求内容摘要、调用purpose与输入校验。
- orange-control-plane：真实PostgreSQL迁移、根预算、已存在Quote的原子提交/来源分桶冻结、同键/同Invocation去重、排队取消与原来源释放、平衡/不可变账本与outbox。
- orange-agent-runtime：真实SQLite持久意图、响应与工具意图原子提交、批准绑定/到期/撤权、独占Host锁、重启后unknown禁止重放；受控本地模型/工具单步循环、上下文恢复、来源重新验证与schema 2迁移，见[Agent控制记录](agent-loop-implementation.md)。真实模型验收仍未通过。
- orange-worker：新增E08核心工程及持久Broker仲裁、独立启动屏障、本地抢占、租约/世代与探针隔离边界，见[Broker实施记录](worker-broker-implementation.md)。设备/身份/云授权为明确替身测试，真实Windows IPC和GPU清理未认证。
- E07 Worker Gateway：真实mTLS/gRPC身份、当前设备/用户/组织鉴权、PostgreSQL会话世代、心跳与撤销已形成切片，见[控制会话记录](worker-session-implementation.md)。测试证书由隔离CA预配置，公开enrollment及任务投递仍未完成。
- 本地开发HTTP入口：预算/Job查询与创建、仅排队任务取消。只允许显式development和loopback；开发令牌不代表OIDC或生产身份。
- orange-model-runtime：固定本机llama.cpp适配、受限SSE/工具参数装配、真实tokenizer接口、取消与上下文边界；17项Rust/SQLite/协议单元测试通过。CPU后端实际构建完成，模型下载因TLS/超时失败，真实推理尚未验证，详见[模型实施记录](local-model-implementation.md)。

Admin 已新增独立 OIDC/MFA、当前授权/会话、总览和操作列表后端；真实本地验证与限制见 [身份实施记录](admin-identity-implementation.md)。

普通用户OIDC与独立Web会话、零余额个人身份、组织创建/列表/详情已连接真实用户Web；会话到期与业务提交使用同一事务，停用用户仍可结束自身会话。证据与边界见[用户身份记录](user-identity-implementation.md)。用户与Admin共享协议验证组件，不共享client、cookie、权限或会话表。

团队角色与独立资源ACL已完成内部事务切片及4项真实数据库测试，见[团队授权记录](team-authorization-implementation.md)。持久通知、游标重放和PostgreSQL事务inbox已完成7项真实数据库测试，见[事件队列记录](events-queue-implementation.md)；自动消费者进程、公共用户身份和完整Worker链路仍待接入。

没有实现的部分仍PENDING：生产身份/团队、价格/模型注册到Quote的真实计价、模型执行、Job运行中取消/结算、Worker/实际GPU、完整工具/连接器、Web云Host、支付、Admin写操作/独立批准及G4—G6。当前HTTP不会伪造这些功能成功。

## 本机运行

先安装PostgreSQL17并运行`python3 scripts/local-postgres.py start`，创建的集群仅位于本仓库.local/postgres且监听127.0.0.1:55432；随机凭证写入忽略目录，不输出到日志。停止使用`python3 scripts/local-postgres.py stop`。

验证：`python3 scripts/check.py --postgres`。脚本明确使用已安装Rust工具链并检查rustc 1.97.1，运行格式、单元/协议/SQLite恢复、Clippy和实际PostgreSQL并发测试。缺数据库时集成测试失败，不悄悄跳过。ORANGE_RUST_TOOLCHAIN可选择同版本已安装工具链。

HTTP二进制orange-api还需显式ORANGE_ENV=development、DATABASE_URL及ORANGE_DEV_IDENTITIES_FILE。身份文件是owner_id/token数组，令牌至少32字符；owner和Quote必须来自受控开发初始化，未提供公开充值/造余额/伪报价接口。真实身份/报价切片完成前，本入口仅用于开发验证。

## 版本与环境记录

当前锁定Rust 1.97.1、SQLx0.9.0、Axum0.8.9、Tokio1.54.1；rusqlite从候选0.41.1调整为0.39.0，因为SQLx工作区与其sqlite3原生links范围冲突。实际依赖版本以Cargo.lock为准，兼容变更记录保留。

Homebrew在当前macOS上安装PostgreSQL时服务注册失败；使用实际安装二进制及补齐缺失的share/lib链接后，专用集群成功启动并完成真DB测试，没有注册系统启动服务。安装过程更换的OpenSSL默认链接已恢复为原OpenSSL3，PostgreSQL继续使用其版本化OpenSSL4依赖。

Flutter官方归档清单返回404，官方GitHub稳定分支读取超时，仍为环境阻塞。原Docker.app缺少可执行文件；已安装Colima0.10.3并启动独立orange-agent profile（2 CPU/4 GiB、无主机目录挂载、不切换默认context）。实际Docker29.5.2/Linux检查通过；配置和socket由Colima保存在用户.colima目录，而非最初尝试设置的工作区COLIMA_HOME。

Worker与云RunStore的Protobuf描述符已通过protoc36.2编译，但尚未实现RPC服务。`python3 scripts/check.py --http`会启动临时loopback API，用明确的测试身份/额度/Quote验证真实HTTP与PostgreSQL链路，然后退出该API进程；不能将其当作真实身份、报价引擎、模型或支付认证。

没有生产服务、商户、真实GPU或完整G3/G4—G6验证。剩余工作在统一台账中保持PENDING，不因这几个切片通过而把原122父任务标完成。
