# 实施状态与本机验证

日期：2026-11-07。四仓库G0—G6仍在实施，**不是完整G3或G6候选版**。准确源码/测试状态以`services_plan/execution/`台账为准。

## 最新增量（优先于下方早期记录）

- [实际网页模型验收](web-model-acceptance.md)已通过：真实Chrome连接API/Worker/固定CPU模型与独立Counter，核对计量、一次平衡结算、CPU关闭及源删除后拒读/清屏；会话/价格/准入为明确夹具。与正式Web22客户端/4浏览器替身、严格Clippy和全部关联回归分别留证。完整Web Agent、支付及未接受提交的确定性撤回不因此完成。

- [当前用户任务查询](user-job-queries.md)已通过统一回归：只返回原提交者的有界任务列表，并按Invocation只读核对原Job和quote_id，不新增输入绑定或执行。PG0027增加游标索引并已冻结；同组织其他管理员、未知参数、源删除和撤权边界通过。USER-JOB-QUERY-001单独DONE，模型任务界面尚在独立准备目录。

- 已实现并完成统一回归：[当前用户取消](user-job-cancellation.md)与[个人账户API](personal-account-api.md)。取消运行中Job保留原冻结额，实际退出/清理后才终结；迟到取消保留已交付结果与收费。个人钱包/预算只归当前会话的个人owner，预算创建不增加余额。正式源码4项取消协议、2项实际模型、3项个人账户测试、严格Clippy及全部关联回归通过，USER-JOB-CANCEL-001与PERSONAL-ACCOUNT-001单独记DONE。一次清理脚本环境键错误已修复并保留失败/复验记录。完整Web任务页面、组织预算治理和支付未因此完成。

- CPU-CLEANUP-001及全部关联回归已通过并提交`3e37cc1`：4项协议/真库、3项实际模型/第二Job/恢复/回收、34项本机测试。PG0025/0026已应用并冻结，执行日志SQLite6。资源/结算事实分别记录；不是GPU或恶意主机远程证明。

- 新增[云端CPU清理与终态](cpu-cleanup-lifecycle.md)：原执行/实例的持久退出事实、独立清理能力、审核CPU配置、幂等云回执和attempt关闭；真实第二个有价任务已执行。未交付任务释放原冻结额而不伪造usage，终态pin精确释放，源删除后的派生对象保留其他消费者引用并最终实际回收。针对性验证通过，最终统一状态以CPU-CLEANUP-001为准；不代表GPU/TEE或生产准入认证。

- 新增[可信文本计量与开发积分结算](trusted-text-pricing.md)：服务端拥有固定模型Counter、离线有界工具schema校验、当前会话报价API、不可变价格/输入快照、发布前可信usage fact及原子开发账本。7项真实计量/结算专项、严格Clippy及全部关联回归通过，TRUSTED-PRICING-001已单独记DONE；包含伪造usage拒用、超限、价格撤销、事务故障/并发恢复及实际多进程闭环。0024已应用；生产价格治理、真实资金、云端cleanup与更多支持组合仍未完成。

- 新增[Worker响应交付](worker-output-delivery.md)：独立mTLS输出能力、有界真实字节、不可变精确版本与ResponseCommit、当前用户/源ACL读取、重复去重和仅查询恢复。实际Worker默认要求输出能力，User API可与Gateway共享单writer；13项输出专项、71项真实控制、5项实际进程/交付、8项真实CPU及全部关联回归通过。0022—0023已应用，执行日志schema5；WORKER-OUTPUT-001已单独记DONE，并修复新增迁移的编译跟踪。Job执行成功不等于账本结算或云端资源回收，后两者仍未完成。

- 新增[开发Worker进程与节点停止接线](worker-development-host.md)：实际Unix二进制、真实UID/私有模型锁准入、mTLS控制和固定模型、信号取消并等待退出、无正文版本化诊断事件。CPU执行者会先停模型，再认证并应用当前节点策略；命令缺失不伪造应用记录。8项真实CPU/策略、2项实际二进制、33项本机测试、58项控制测试和全部关联回归通过，WORKER-HOST-001已单独记DONE。候选仅在内存中未提交，云Job结算与Admin完成状态不自动改变；正式安装/恢复/输出仍待实现。

- 新增[请求绑定的云执行续期](worker-execution-renewal.md)：独立版本协商、不可变首次决定、当前会话/输入/模型/预算授权、精确Broker许可和单调发送锚点。7项续期专项、58项真实控制测试、30项本机Broker/日志测试、5项真实模型测试与完整关联回归已通过，EXECUTION-RENEWAL-001已单独记DONE。0021增加有效期记录，原Assignment不变；本机schema4将未确认续期恢复为unknown。输出、结算和正式常驻控制器仍未完成。

- 新增[受管 CPU 单任务执行](worker-cpu-execution.md)：真实输入→启动许可→固定模型生成，独立心跳/当前授权和 Broker 监督，显式取消/源对象删除/Handle 丢弃后的实际进程退出。4 项真实模型路径、29 项本机 Broker/日志测试、51 项既有控制测试和全部相关统一回归均已通过；WORKER-CPU-001 已单独记 DONE。SQLite schema3 保存唯一实例和恢复 unknown，不重放效果。当前候选未提交、云 Job 未结算，尚无云续期、常驻服务、OS 崩溃自动恢复或 GPU 认证。

- 新增[用户输入绑定与Worker租约读取](worker-input-delivery.md)：用户Job发起归属、显式团队预算授权、不可变输入/pin、当前权限与原请求投影校验、协商的mTLS分块读取、只读Gateway与客户端完整性校验。51项Worker及16项对象测试通过，相关统一源码回归全部刷新，WORKER-INPUT-001单独记DONE。0020已应用；当前只有文本GenerateRequest输入交付，没有模型执行/输出/结算认证。

- 新增[私有输入对象存储](artifact-storage-implementation.md)：OpenDAL0.59.5、实际字节上传/校验/发布、当前会话与独立ACL、权限版本隔离、不可变版本、HTTP接口及引用保护回收。14项真实PG/文件系统/HTTP测试、实际开发二进制探测及全部相关源码回归通过，ARTIFACT-STORE-001单独记DONE。0017—0019已应用并冻结；Worker输入授权、推理输出与结算仍待接线。

- E07/E08新增[拒绝持久回执与历史查询](worker-rejection-receipts.md)：独立可选能力、本机决定/云确认分离、当前节点认证、精确原ACK、schema1升级、旧PG事实补录及事件幂等。47项真实控制测试、28项本机测试、Rust/集成特性Clippy和相关统一源码回归全部通过，REJECTION-RECEIPT-001单独记DONE。拒绝不代表退款、设备释放、实际模型执行或自动重派。0016已应用并冻结。

- E08新增[持久接收与本机启动屏障](worker-execution-receiver.md)：显式能力2、WAL/FULL执行日志、准确Broker许可、首次ACK单调锚点、不可延长预算及新鲜确认。39项真实控制测试、25项本机Broker/日志测试和Rust/Clippy通过；跨提交期限负例先红后绿，修复后的统一源码回归已全部刷新，EXECUTION-RECEIVER-001单独记DONE；实际模型进程/输入对象/续期/清理/结算仍未实现。迁移0015已应用并冻结。

- 已新增[E07执行许可屏障](worker-execution-barrier.md)：显式能力协商、持久分配、精确ACK/StartGrant绑定、取消竞争、断流unknown和持久Job事件/outbox。手动32项实际mTLS/PostgreSQL回归通过，包含11项新的执行屏障测试；协议新增2项负例/摘要测试，Rust格式与Clippy通过。修复后的统一源码回归已全部通过，EXECUTION-GRANT-001已单独记DONE，原父任务仍未整体完成。
- E08默认客户端继续执行能力0。审核/Quote调度写入、执行日志与模型Host、输入对象、renew/resume、结果/清理/结算仍未完成，不以测试ACK或running状态当真实推理成功。
- 固定Qwen完整权重已取得；实际CPU模型→批准→真实文件→结果→恢复已通过，见[真实模型验收](real-model-agent-acceptance.md)。原模型下载阻塞是历史情况。
- 桌面已有固定Runtime版本、实际Flutter/FRB/SQLite、受管模型导入/启动/停止和产品按钮验收；桌面仍消费376f883源码包，服务端本轮改动不会隐式进入桌面。
- Admin独立身份/MFA及三类节点意图、独立批准、审计、真实控制命令/回执/本机停止策略已完成局部切片，见[Admin动作](admin-actions-implementation.md)、[节点策略](node-policy-implementation.md)。其他Admin动作及设备恢复仍未完成。
- Flutter/macOS与专用Colima运行环境已可用。Windows/NVIDIA、多卡、多域、长期观察与生产认证尚无完整证据；其余工具/连接器、云Host和G4—G6仍有实现工作。

## 早期基础切片记录

以下保留初始实施时的环境与范围记录；其中“未通过/未实现”应结合上面的新增切片及具体实现文档判断，不代表当前全部能力。

## 已实现

- orange-domain：整数金额、规范字符串、计价/费率、预算限额、取消/完成互斥、旧响应和GPU回收事实检查。
- orange-protocol：严格DTO、JSON金额无损传输、请求内容摘要、调用purpose与输入校验。
- orange-control-plane：真实PostgreSQL迁移、根预算、已存在Quote的原子提交/来源分桶冻结、同键/同Invocation去重、排队取消与原来源释放、平衡/不可变账本与outbox。
- orange-agent-runtime：真实SQLite持久意图、响应与工具意图原子提交、批准绑定/到期/撤权、独占Host锁、重启后unknown禁止重放；受控本地模型/工具单步循环、上下文恢复、来源重新验证与schema 2迁移，见[Agent控制记录](agent-loop-implementation.md)。真实模型验收仍未通过。
- E03 schema3增加持久工作区授权、物理目录身份、关联Run撤销和无正文状态分页，见[工作区授权记录](workspace-consent-implementation.md)；桌面消费版本只有在新源码发行包固定导入后才更新。
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

统一执行DAG使用`check.py --postgres/--teams/--events/--queue/--http --focused`执行指定专项，完整workspace单元与Clippy仍由同一源码的DOMAIN-001前置负责；未删除任何专项断言。默认、不带`--focused`的独立调用保留原完整检查。focused不得无目标运行或改写源码，也不能单独宣称通过全工作区验证。

HTTP二进制orange-api还需显式ORANGE_ENV=development、DATABASE_URL及ORANGE_DEV_IDENTITIES_FILE。身份文件是owner_id/token数组，令牌至少32字符；owner和Quote必须来自受控开发初始化，未提供公开充值/造余额/伪报价接口。真实身份/报价切片完成前，本入口仅用于开发验证。

## 版本与环境记录

当前锁定Rust 1.97.1、SQLx0.9.0、Axum0.8.9、Tokio1.54.1；rusqlite从候选0.41.1调整为0.39.0，因为SQLx工作区与其sqlite3原生links范围冲突。实际依赖版本以Cargo.lock为准，兼容变更记录保留。

Homebrew在当前macOS上安装PostgreSQL时服务注册失败；使用实际安装二进制及补齐缺失的share/lib链接后，专用集群成功启动并完成真DB测试，没有注册系统启动服务。安装过程更换的OpenSSL默认链接已恢复为原OpenSSL3，PostgreSQL继续使用其版本化OpenSSL4依赖。

Flutter官方归档清单返回404，官方GitHub稳定分支读取超时，仍为环境阻塞。原Docker.app缺少可执行文件；已安装Colima0.10.3并启动独立orange-agent profile（2 CPU/4 GiB、无主机目录挂载、不切换默认context）。实际Docker29.5.2/Linux检查通过；配置和socket由Colima保存在用户.colima目录，而非最初尝试设置的工作区COLIMA_HOME。

Worker与云RunStore的Protobuf描述符已通过protoc36.2编译，但尚未实现RPC服务。`python3 scripts/check.py --http`会启动临时loopback API，用明确的测试身份/额度/Quote验证真实HTTP与PostgreSQL链路，然后退出该API进程；不能将其当作真实身份、报价引擎、模型或支付认证。

没有生产服务、商户、真实GPU或完整G3/G4—G6验证。剩余工作在统一台账中保持PENDING，不因这几个切片通过而把原122父任务标完成。
