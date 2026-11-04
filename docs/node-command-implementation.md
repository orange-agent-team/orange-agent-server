# 节点命令传输与持久接收

2026-11-05。范围是 E02/E07/E08 的控制命令接收切片；没有接通本机执行、进程清理、GPU回收或节点恢复。

## 身份、版本与能力

Worker v1 Hello/Welcome 追加 `node_control_version`。0表示旧客户端，不发送节点命令；只有双方明确协商1才可收取 NodeCommand/发送 NodeReceipt。未知能力版本与未知命令类型拒绝。原 Assignment/StartGrant/模型执行帧仍未开放。

NodeStopIntent只包含command/action/node、独立policy_generation及drain/quarantine类型。generation在JSON中是正的规范十进制字符串，上限PostgreSQL BIGINT，不经过JS浮点数。摘要使用固定`orange-request-v1`域`node-stop-intent-v1`；会话session/connection_epoch放在传输信封，重连不会改变原命令摘要。

迁移0011新增独立策略generation。已存在且尚未发送的命令按节点target_version排序补齐generation，保留原ID。Admin入队与generation递增同事务；控制重连、心跳与普通节点version变化不修改策略generation。迁移0012增加协商能力、发送/接收事实、不可变命令身份与Worker命令事件。成功应用的既有迁移字节没有修改。

## 云端与本机提交边界

云端每次发送/接收重新检查真实mTLS证书映射、当前node/owner/登记用户/组织权限、Gateway/session/epoch、协商能力与控制期限。发送前先提交dispatched，再向有界流写出。当前策略generation必须匹配。旧的未发送策略可标failed；已发出但断线、超过15秒或被新策略取代的记录标unknown，不自动重新发送。未知不会变成成功或释放资源。

NodeJournal使用私有目录、独占锁、SQLite WAL/FULL、不可变intent与稳定receipt_id。其配置绑定单个node；旧连接世代、同世代不同session、旧策略、同ID不同内容、同generation不同命令和错误摘要拒绝。接收只有在本机事务提交后才生成回执，写盘失败不会确认。重启可读取原收据，但读取不触发执行。

云端回执必须匹配原command/node/generation/digest和发送时session/epoch，仍检查当前凭据和接收期限。相同回执只产生一次审计；不同receipt_id不能覆盖原记录。收到回执仅将命令置为received，AdminAction仍executing，节点仍draining/quarantined。本机当前没有把回执接到Broker策略钩子，不能说已经停止远端任务。

| 事实 | 已能证明 | 不能据此推断 |
|---|---|---|
| queued | 管理控制事务保存了意图 | Worker在线或已接收 |
| dispatched | 云端已提交发送尝试 | 对端收到、任务停止 |
| received | 当前认证Worker报告可靠落盘 | Broker已应用、进程退出、GPU可用 |
| unknown | 发送/回执结果需要按原编号核对 | 可自动重发或直接结案 |

## 实际验证与限制

`python3 scripts/check-node-commands.py` 运行9项协议、17项Worker库（14 Broker+3命令日志）、18项真实mTLS/PostgreSQL集成测试及启用集成测试的Clippy。覆盖旧客户端不收新命令、未知能力拒绝、真实E08客户端落盘、连接世代/策略代隔离、重复回执、不同摘要/receipt_id拒绝、回执丢失不重放、加速超时、凭据撤销和不可变证据。加速超时使用明确数据库时钟夹具，不称为长期运行观察。

证书和已接受Admin意图是隔离夹具；TLS传输、数据库、SQLite日志是真实实现。Admin的HTTP/MFA生产者在独立切片中验证，本次不把它称为完整Admin→设备清理端到端。Windows/GPU、公共enrollment、密钥保护和生产网络仍未认证。

下一步：由可信Worker Host核对当前策略后调用Broker停止远端供给，绑定本机监督器与实际清理探针；为unknown提供原命令/收据核对路径。恢复必须有可验证的资源状态，不能直接信任NodeReceipt、远端布尔值或health_report_id字符串。模型任务的StartGrant还必须与获准Broker grant及slot generation逐一绑定，避免复用旧云授权。
