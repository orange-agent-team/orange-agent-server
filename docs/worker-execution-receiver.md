# E08持久接收与本机启动屏障

2026-11-05。已实现真实Rust接收器、mTLS/PG交互和私有SQLite/Broker状态转换；本切片没有启动推理进程，不能作为GPU回收或结算认证。

## 调用顺序与信任边界

默认`ControlConnection::connect`与节点策略入口继续使用执行能力0；显式`connect_execution`同时协商节点策略2、执行能力2，并绑定NodeJournal和ExecutionJournal到同一节点、会话与连接世代。不存在从外部JSON直接构造AuthorizedExecution的入口。

1. `poll_execution`用有界心跳交换接收Assignment和节点命令。校验节点/会话/世代、规范摘要、策略世代；节点命令仍按既有WAL/FULL回执流程处理。最多16个帧，超时或异常后连接不可继续用作授权。
2. Assignment先写ExecutionJournal。零输入对象限制保留；对象交付和ACL未接通前，不能接受任意artifact ID或运行真实请求。
3. `reserve_execution`同步调用Broker，不在网络等待中持有Broker借用或事务。必须匹配本机批准的profile ID、模型SHA256、执行配置摘要、已登记节点、当前策略和用户共享开关。
4. Broker真实grant先提交，再在ExecutionJournal记录同一个grant/epoch/slot generation。缺任一提交，不产生accepted ACK。原入口资源繁忙/配置不符保持错误；新增显式拒绝能力入口可持久负ACK并取得回执，详见[拒绝回执](worker-rejection-receipts.md)，不自动释放占用。
5. `authorize_execution`先将日志推进ack_sent，再记录第一次发送ACK的单调时钟。不可重新调用来重建时钟起点；中断/丢弃Future会使连接失效，日志保留不确定边界。
6. StartGrant须逐字段匹配原ACK、Lease和本机grant，并提供1001—45000ms预算。再完成一次实时心跳/策略核对，返回借用当前连接的不可序列化AuthorizedExecution；确认窗口最多5秒。
7. `confirm`先持久写start_claimed，再调用Broker ConfirmStart，最后写running。两份持久状态提交后再次检查确认期限和运行期限，防止磁盘等待跨过授权期限。任一步失败不返回StartedExecution；Broker已确认而日志提交失败时进入draining，不能把资源变为idle。
8. StartedExecution只证明本机启动屏障通过，尚不是模型启动或结束证明。首次监督窗口最多14秒（Broker初始15秒扣安全边界），并受原云期限/控制会话期限约束；后续Host必须运行实际监督、续期、撤权和退出处理。

收到节点意图不是已应用策略。调用方仍需用已有`authorize_node_stop(...).apply(broker)`执行当前策略；正在执行的Host还必须停止并排空受管工作。此处没有常驻Host，所以不能宣称节点动作已完成物理清理。

## 两个持久库及失败语义

ExecutionJournal采用独占文件锁、私有目录/文件、SQLite WAL/FULL，拒绝未知格式、非空无版本库及跨节点打开。命令/Attempt/日志ID和分配字节不可改写，已记录的grant不可替换，也不能删除历史。

状态为received→reserved→ack_sent→start_claimed→running；重开进程或绑定新连接时，所有未解决记录改为unknown。只读`record`不会重放效果，也不是授权令牌。Broker是资源占用的唯一裁决者；Journal不把unknown或崩溃视作资源已释放。

两个SQLite库不声称跨库原子事务：Broker预留成功但Journal失败会保留预留，等待超时/核对；Journal start_claimed后Broker拒绝则不启动；Broker确认后Journal失败则排空。用户态模型监督器接入时必须沿用此顺序，不能绕过它直接使用一个Grant DTO。

## 不可延长的时钟

迁移0015新增`start_budget_ms`：服务器在首次接受ACK时计算原执行Lease剩余毫秒并持久保存，后续相同ACK返回完全相同的StartGrant预算，绝不返回新的45秒。

客户端计算`first_ack_send + start_budget_ms - 1000ms`，再与控制期限取最小值；这会扣除发送、服务器处理和返回延迟。Budget不是以接收时间为起点。单调锚点不跨进程保存或根据墙上时钟重建；原锚点丢失、重复ACK或未知状态均不能重新启动。

ConfirmStart的5秒新鲜度与实际运行期限分开：过期的授权对象即使仍持有引用也不能确认；确认后返回的首个运行监督期限仍不超过本机14秒和原云期限。暂未提供运行续期/恢复，不能靠心跳延长执行。

## 验证

```sh
python3 scripts/check-execution-receiver.py
python3 scripts/check.py
```

本机手动通过39项mTLS/PostgreSQL测试（其中7项真实E08接收器测试）及25项Broker/NodeJournal/ExecutionJournal测试（新增3项日志测试），并通过Rust格式、单元及Clippy检查。真实测试包含落盘先于ACK、本机确认晚于StartGrant、完整摘要、原始预算消耗、重复发送拒绝、重启unknown、本地共享撤销、等待Future中断、授权过期和running写入失败后的排空。既有32项控制/云端许可回归保持通过。

证据边界：网络、数据库、SQLite和Broker是真的；证书登记、审核目录/Quote计划、测试额度、OS进程身份和设备清理探针为明确夹具。测试没有推理进程，没有真实GPU或真实收费。最终源码摘要和统一回归以services_plan/execution为准。

迁移0001—0015均已在本机开发库应用，不修改字节。依赖仅增加已经锁定的prost/prost-types/rusqlite直接引用，没有升级版本。桌面固定Runtime376f883仍独立，不隐式消费本次变化。

## 下一步

- 资源/协议拒绝ACK及拒绝历史查询已有独立切片；继续断开/撤权的常驻排空和正向未知命令核对。
- 实现真实输入对象所有权/摘要/交付授权、审核目录与Quote/调度写入；当前零artifact限制不能用于假装完成输入传输。
- 接入实际CPU模型监督器、Broker监督续期、云执行租约续期/恢复和有界取消。保持公共Worker只做审核推理，完整Web Agent在私有隔离云工作区运行。
- 结果提交屏障、DR-01计量、物理清理证明、账本结算与异常恢复；之后才扩展GPU、目标OS、多卡和后继阶段。当前仍有实现工作，不能全登记为外部阻塞。

代码复核新增的真实SQLite写锁跨期负例先红后绿：旧逻辑发出过期启动凭据，增加提交后期限检查后39项全部通过。证据见services_plan/execution/evidence/execution-receiver-deadline-review.json，断言未弱化。Host还需接入真实睡眠/唤醒、设备变化与进程生命周期失效通知；现有Broker失效钩子不代表OS监控已经部署。

后续增量：ExecutionJournal schema2、可选rejection_receipts_version1及负ACK恢复见拒绝回执记录。当前网络回归47项、本机Broker/日志28项；0016已应用，旧版本记录见原时点证据。
