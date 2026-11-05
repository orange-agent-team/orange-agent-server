# Worker拒绝决定、云端回执和历史查询

2026-11-05。E07/E08已实现真实mTLS/PostgreSQL/SQLite负ACK闭环。此处“拒绝已确认”不代表Job完成、费用退回、设备已回收或任务已重新调度。

## 可选能力和接口

Hello/Welcome独立协商`rejection_receipts_version=1`，并将其写入当前Worker Session，要求执行能力2、节点策略能力2。旧入口与旧消息保持该字段为0，负ACK不额外返回新帧。新入口`connect_execution_with_rejections`严格要求服务端回显能力；不会把不支持能力的旧服务端当成已支持。

| 接口 | 行为与完成条件 |
|---|---|
| `prepare_execution` | 返回准确Broker预留，或先持久写入本机拒绝决定；不在此进行网络确认 |
| `send_rejection` | 从日志读取原拒绝ACK，只能在原会话发送；云端提交后返回RejectionReceipt，本机提交回执后才成功 |
| `query_rejection` | 在当前认证会话中查询原负ACK，可使用新连接；只能读取已经提交的拒绝事实 |
| `RejectionReceipt` | 原command、payload digest、完整历史Lease、精确拒绝类型和不可变receipt ID；不是启动或清理许可 |

只有已知且未产生新grant的拒绝才会成为负ACK：本机型号/执行配置摘要不符为ProtocolRejected；资源占用、共享关闭、平台停止或待核对为ResourceRejected。Supervisor身份、节点归属、策略世代、磁盘或其他不确定错误保持错误，不转换为“普通拒绝”。检查本机身份和节点策略先于配置匹配，避免用配置不符掩盖已失效身份。

原有`reserve_execution`仍保持错误返回语义，默认`connect_execution`不启用新能力。调用者必须明确使用新能力入口及`prepare_execution`，不能仅修改Hello的数字而绕过持久日志。

## 持久化与恢复

ExecutionJournal schema2新增不可改写的原负ACK、类型和云回执。schema1升级保留原grant、命令与日志ID，将未解决正向工作保持unknown，不捏造旧记录的拒绝证据。拒绝决定一旦持久化，就不能再预留或发送accepted ACK。

本机`state=rejected`只表示“本机已决定拒绝”；`RejectionView.receipt_id=None`表示云端尚未确认，Some才表示经过认证的云回执已经本机落盘。重开日志保留决定和回执，不能把连接丢失或缺少回执当成云端成功。

正常负ACK在E07原会话、租约、10秒接收窗口、当前节点/模型/预算/Job校验通过后，同事务写入拒绝、精确ACK摘要、不可变云回执、Worker审计及`job.assignment_rejected`事件/outbox。重复原ACK不会重复事件、改变类型或产生StartGrant。原本的排队Job与资金冻结保持不变。

历史RejectionQuery只用原负ACK读取本节点已提交事实：当前mTLS会话/设备/用户授权必须有效，原ACK摘要、Lease、command和类型须完全匹配。它可以在重连、原Lease过期或Job后来取消后取得历史回执，但不更新原Lease、不重放任务、不变更Job/钱包，也不把offered/started/unknown补成rejected。没有可确认事实时明确失败。

对于迁移0016之前已经拒绝但没有回执列的记录，只有匹配原持久ACK摘要后才可补录receipt ID和一次回执审计。补录不产生新的Job事件。错误类型、跨节点或尚未提交的拒绝均不能通过查询伪造回执。

回执丢失或本机回执提交失败时，关闭失效连接、重开日志、建立新认证会话，然后调用`query_rejection`；不能在新会话重发旧ACK来伪装成新执行命令。若云端从未提交负ACK，查询不会自动消除unknown，后续仍需核对流程。

## 资源和资金边界

拒绝一个远程任务不会停止、释放或覆盖本地任务的Broker grant。ResourceRejected也不表示整个设备idle。回执类型不能传给任何GPU清理或账本结算接口。

目前仍是每Job一个Attempt，没有自动重派。拒绝后Job继续queued且余额冻结；原有显式排队取消可以零费用、按原资金来源释放。实际自动重试/重新调度、运行中取消、执行输出、物理清理和结算需要继续实施，不能以本切片替代。

## 验证

```sh
python3 scripts/check-execution-receiver.py
python3 scripts/check.py
```

手动47项真实mTLS/PostgreSQL测试、28项真实SQLite/Broker/日志测试及Rust/Clippy通过，其中新增8项回执链路测试和3项本机迁移/拒绝测试。测试覆盖本地持有者保留、资源与协议拒绝、重复事实、回执写入失败/重连/取消后查询、跨节点、未提交状态、旧客户端、失效Supervisor、旧PG记录补录、schema1升级、原类型/receipt ID及不可变历史。

测试的网络、数据库、SQLite与Broker是真的；证书登记、OS身份、设备探针、审核配置/Quote和额度为明确夹具，没有实际Worker推理、GPU或真实资金认证。统一台账绑定最终源码摘要，测试不能证明尚未实现的完整Host或G0—G6。

迁移0001—0016及已提交ExecutionJournal schema1保持原字节，新增schema2升级文件。未新增依赖版本、付费资源、公开服务或远端发布。

## 接续工作

下一步接常驻Worker控制事件循环、断流/撤权排空和实际进程监督，并完成真实输入对象权限、审核目录/Quote计划写入。现在等待特定回应期间遇到其他控制帧会拒绝并关闭不确定会话；完整Host需统一分发并处理节点命令，不可丢弃它们或假报管理员动作已完成。随后实现执行续期/恢复、输出提交、DR-01计量、真实清理/结算与有界重派。公共Worker仍只运行批准的推理，完整Agent工具工作放在平台隔离云工作区。
