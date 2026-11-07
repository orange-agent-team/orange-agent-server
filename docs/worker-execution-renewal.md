# 请求绑定的执行续期

本切片在受管 CPU 执行之后增加真实云租约续期。仍没有输出交付、ResponseCommit、账本结算、任意孤儿恢复或正式常驻服务安装；实际模型候选继续保持未提交。

## 协议与服务端

Hello/Welcome 独立协商 `renewal_version=1`，要求既有 input_artifacts_version1、execution_control_version2、node_control_version2。旧客户端继续原不可延长租约。新增 unary `RenewExecution` 与控制流共享 mTLS 设备身份和当前会话，但不占用控制流的响应收件器。原草案 `Renew/ServerFrame.renewed` 仍拒绝，不将其解释为有效许可。

请求包含当前 session、唯一 request_id、单调 sequence、原 accepted ACK（包括不可变 Assignment/Lease、journal、Broker grant/epoch/generation）及 supervisor instance。规范摘要使用 `worker.execution.renewal.v1`，序号用十进制字符串防止大整数丢失。原 Assignment/Lease 保持不可变；PG attempt 的有效期限单独更新，输入读取同时验证原身份与数据库中的当前有效期。

迁移0021新增协商列、attempt续期序号/首次监督器绑定和不可修改的决定表。事务采用现有节点/会话→用户/预算→Job→attempt顺序，复核当前节点策略、模型审核配置、原 Quote 请求、Job running、预算和输入权限。过期、unknown、撤权或已删除输入的任务不续期。

新请求必须为下一序号；每个attempt最多8640次，相邻新请求至少5秒。重复 request_id 只在完整摘要/序号一致且原决定仍有效时返回首次 budget，不更新期限或生成第二个事件。第一次监督器身份固定后，其他实例不能继承。新会话即使复用设备证书，也不能承接旧会话的执行。

每次最多45秒，受当前控制会话及原 Job deadline 限制；更新必须发生在旧执行仍有效时。首次预算、到期时间和事件一并持久化。提交后再次确认设备会话、策略和决定有效期，提交等待不能把历史决定当作新许可。

## 本机客户端与 CPU 所有者

`connect_renewable_execution` 显式开启能力。`prepare_renewal` 在网络请求前将 request/sequence/digest 写入 SQLite schema4，并保留非序列化、不可 Clone 的首次单调锚点。存在 pending/unknown 请求时不能创建新锚点。重开将 pending 改为 unknown；不会用重启后的接收时间恢复许可。

`renew_execution` 只接受匹配 request_id/sequence/digest、限定预算、Job deadline内有效时间的响应；等待上限为首次锚点后5秒和当前会话期限的较小值。授权期限从首次锚点计算并扣1秒，不能按响应到达时间重新获得45秒。

`confirm_renewal` 再次核对当前连接、原 execution record和同一个Broker grant，持久确认后调用Broker.renew，并在写入后检查本机期限。另一grant、已排空或过期的槽不能被新响应复活。已确认响应不可重复消费；网络/日志不确定就停止而不自动重发。

CPU所有者每秒维持本机监督、每两秒检查节点及输入授权、成功后每10秒申请云续期。网络在独立Future中执行，不持有Broker事务；本机监督、模型状态及取消仍被轮询。取消打断续期可能留下pending记录，所有者仍必须等待实际模型退出后才能释放本机槽，不能把云端记录不确定当资源空闲证明。

## 实测与界限

`python3 scripts/check-worker-renewals.py` 使用真实本地PostgreSQL/mTLS/SQLite/Broker验证首次、重复、冲突、限频、过期、删除、旧版本、会话替换以及本机pending不可重置；初始目录/价格/额度/设备准入仍为明确夹具。

`python3 scripts/check-real-worker.py` 运行真实固定模型。除生成、取消、输入删除和Handle丢弃外，还将实际受管模型有意保持空闲48秒，在持续心跳/续期/本机监督下跨过不可变原租约期限，再执行真实生成并验证进程退出和继承锁释放。它证明真实进程及执行许可可跨原窗口，不是长时间生成吞吐量、生产SLA或GPU测试；该用例不以替身探针声明Broker清理完成。

当前实现只自动续期同一会话和原始执行。会话重连/宿主重启后的恢复、正式节点策略应用/常驻入口、资源清理上报、输出与结算仍需后继实现。真实目标OS、GPU、多域和长期运行认证保持独立状态。
