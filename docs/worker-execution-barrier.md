# Worker 云端分配与启动屏障（能力版本1）

本切片为E07的真实PostgreSQL/mTLS执行许可实现，尚不是端到端推理、设备回收或计费结算。默认E08控制入口仍协商能力0；后续已新增显式能力2的真实接收器和本机启动屏障，见[接收器记录](worker-execution-receiver.md)。原能力1测试仍使用显式ACK替身，不作为模型Host发布。

## 数据来源与权限

- Hello新增独立执行能力，Welcome精确回显；0不会接收Assignment，未知版本拒绝。节点管理版本不授予执行能力。
- `reviewed_execution_profiles`保存不可修改的模型版本/文件SHA256/配置摘要，可单独撤销enabled。`quote_execution_plans`把既有Quote、指定目标节点、配置、请求投影和任务期限不可变地绑定。
- 上述表是可信审核/Quote/调度模块的内部输出，没有公开写入HTTP/Worker接口。当前只有隔离测试夹具写入，实际审核目录、价格引擎与自动调度仍待实现。
- 当前输入对象交付未完成，因此迁移强制输入对象列表为空，不能以任意artifact ID绕过访问检查。原始SubmitJob摘要必须重算匹配；Quote所有者/预算/调用编号/最大金额须匹配Job冻结记录。只有计划投影与Quote一致、Job仍在排队、预算/所有者可用、节点在线且配置有效时才可分配。
- 认证来自真实mTLS叶证书和当前数据库会话，不信任报文里的node ID。每个操作重新锁定节点、核对证书/用户/成员资格/连接世代；Job访问使用owner→budget root→Job锁顺序，与取消共用Job锁。

## 协议与状态

1. 一个Job目前最多一个Attempt；每个节点最多一个未解决Attempt。分配与不可变事件同事务提交，之后才发网络帧。
2. Assignment携带command/lease/job/attempt、连接世代、全局单调fencing token、节点/会话/策略世代、模型/配置摘要、请求投影和期限。摘要采用`orange-request-v1`、操作名`worker.assignment.v1`，整数投影为十进制字符串，避免2^53精度损失。
3. 初次分配租约最多45秒（整秒向下取整），接收确认窗口最多10秒，受Job期限约束。约2秒可重发同一Assignment，所有ID、内容和截止时间保持不变。
4. 接受ACK必须逐字段匹配原Lease/命令/摘要，包含持久执行日志ID及Broker grant/epoch/slot generation。ACK自述是协议输入，并不是服务器对本机进程、SQLite或GPU的物理认证。
5. 服务端重新核对节点/策略/配置/预算/Job/期限后，在同一事务中保存ACK摘要、将Attempt记为started、Job记为running、写`start.granted`审计与`job.start_granted`持久事件/outbox，然后返回StartGrant。`running`在此表示云端已发启动许可，不保证模型进程已启动。
6. 同一ACK可在原租约内重取完全相同的StartGrant，不能换本机grant、延长租约或重复产生Job事件。拒绝ACK不携带本机执行证据，不发StartGrant，不能事后改成接受。
7. `cancel_queued`与StartGrant竞争同一Job锁：取消先提交则不发许可，零扣费释放原来源；许可先提交则排队取消拒绝，保留冻结，等待将来的运行中取消/清理/结算流程。
8. 断流、旧会话、策略变化或期限丢失转为unknown，保留节点占用和资金责任。即使没有收到StartGrant回包，也不能自动重派或假定设备空闲。重连不恢复旧许可。

只有云端StartGrant和本机Broker ConfirmStart同时有效才能启动模型。E08持久日志、接收/ACK、保守时钟和ConfirmStart已由显式能力2入口连接，详见接收器记录；实际模型监督器仍待连接，不能把本机running状态当作真实模型执行。

## 可复现验证

```sh
python3 scripts/check.py
python3 scripts/check-proto.py
python3 scripts/check-worker-session.py
```

当前手动32项真实控制链路测试通过，其中11项为执行屏障；协议另有2项摘要与危险输入测试。统一源码回归记录另见执行台账。测试使用真实本机PostgreSQL和临时CA/mTLS；身份、测试额度、审核配置、Quote/调度计划、Worker执行日志/Broker ACK标识是显式夹具。覆盖重传、幂等、旧版本、取消竞争、超时、节点与模型撤销、跨设备/世代及资金保留。不访问用户桌面授权目录，不调用付费模型或资金通道。

迁移`0014_worker_execution_barrier.sql`已在开发库应用，不再修改字节；后续结构变更必须新增迁移。旧迁移0001—0013保持原字节。依赖只增加对已锁定prost的直接引用，没有升级第三方版本。

## 后续实现顺序

1. E08能力2持久接收与启动屏障已实现；继续资源拒绝ACK、常驻Host撤权/断流处理及原命令核对，不重放unknown。
2. 审核配置与Quote/调度生产写入、输入对象归属/摘要/短期交付授权、每租户限流、公平调度；不接受Worker自述的模型或价格。
3. 实际CPU受管模型/执行监督器，租约续期与旧连接输出拒绝、同Attempt恢复、运行中取消及所有任务退出后的清理证据。
4. 响应提交、DR-01计量、实际扣费/原来源释放和供应方收入事务，恢复unknown且不重复副作用；GPU/跨OS另做真实认证。
5. 允许重试和多卡前，完善Attempt序列、设备/slot级唯一占用及清理后的显式释放。当前未知节点/Job不会自行恢复，拒绝的Job也不会自动重派；这是未实现的恢复工作，不是完整生产调度能力。

复核新增的原请求不一致负例先失败（旧逻辑发出了Assignment），补齐SubmitJob摘要重算后32项全部通过。红绿日志及摘要保存在统一执行台账evidence中，未删除或弱化断言。
