# 管理节点操作、独立审批与持久命令

日期：2026-11-05。实现范围为 Admin 的节点操作切片，不能据此标记完整 ADMIN-W03 或 G3 完成。

## 已实现的边界

- `GET /admin/v1/nodes/{node_id}?environment=dev`：实际节点状态和版本；按当前主体的 platform/tenant/region/resource 授权检查，记录读取审计。聚合总览仍只支持 platform/tenant。
- `POST /admin/v1/actions`：只接受三种节点动作的严格载荷。排空与隔离为 single_operator；恢复为 maker_checker，需要另一实际主体批准。其他十四种动作尚未开放。
- `GET /admin/v1/actions/{action_id}`：读取受管理动作及准确 ETag；历史只读投影不能成为执行命令。
- `POST .../approvals`、`.../cancel`、`.../execute`：幂等键、If-Match、完整服务端摘要、固定政策版本、当前作用域权限和最近五分钟 MFA；所有写入均要求 Origin/CSRF。
- 管理操作列表支持四种 scope，除 platform 明确的全平台读取外，其他 scope 必须精确匹配。资源范围不能通过总览接口读取平台汇总。

迁移 `0010_admin_action_control.sql` 新增托管动作、请求键、不可变事件及节点命令表。原迁移没有修改。请求/申请人/摘要/政策/期限不可变；审批、撤回、命令入队与审计均在同一 PostgreSQL 事务内提交。

身份查询支持复用调用者事务，主体、会话、授权锁持有至领域提交。提交前再次检查会话、MFA和授权时间；执行还重新验证申请人以及独立批准人的当前权限。低频管理事务通过一个数据库事务锁统一加锁次序，设置两秒锁等待、五秒语句上限；不在锁内访问模型、Worker 或外部服务。这个保守方案没有完成生产容量认证。

幂等键按主体唯一，绑定动作 ID、操作、请求体与原 If-Match。同键不同内容返回 `IDEMPOTENCY_CONFLICT`；同键重试仍先鉴权，再返回当前投影。版本冲突返回协议中的 `VERSION_CONFLICT`。不把网络失败自动重试为新操作。载荷摘要采用既有 `orange-request-v1` 序列化规则和独立操作域 `admin-action-payload-v1`，由服务端生成，浏览器只展示和回传。

## 设备效果不混同于控制面效果

排空/隔离执行会原子改变节点控制状态、增加节点版本、插入唯一的 `admin_node_commands`，并将动作置为 `executing`。命令仍是 `queued`，绝不直接标成 `succeeded`。当前没有接通 Worker 命令消费者、确认回执、超时核对、GPU/进程排空证据，因此也没有声称设备停止或资源可复用。

恢复申请和独立审批可以保存；恢复执行在完整重新鉴权后返回 `SERVICE_UNAVAILABLE`。单独的 health_report_id 字符串不是可信回收证明。下一切片需要把命令 ID、节点连接世代、Broker 世代、租约及实际清理探针绑定后再开放恢复。

## 可复现验证

```sh
python3 scripts/check-admin-actions.py
python3 scripts/check-admin-identity.py --admin-repo ../orange-agent-admin
```

第一条使用真实 PostgreSQL 与显式会话夹具：七项动作测试加三项身份测试，覆盖同人自批、非授权范围、审批撤权、申请人撤权、过期/MFA/CSRF、未知字段、摘要与版本冲突、并发目标争用、重复入队、请求及事件防篡改。夹具不证明 MFA 发放。共享错误枚举另有契约单元测试。

第二条使用本机 HTTPS Chrome、实际 Keycloak 密码/TOTP、Rust API 与数据库，包含完整节点排空申请 UI，以及两个真实身份的独立审批 API。第二位测试身份的临时 dev 权限在 finally 中撤回；不改变生产身份。TOTP 在新时间步提交，保留身份提供方防重放校验。结果以执行台账对应源码摘要与日志为准，不以本文件的测试描述代替通过证据。

## 未完成事项

命令投递/回执与崩溃核对、可信设备恢复、完整节点列表、其余管理动作、拒绝审计与入口限流、过期状态自动投影、生产部署与容量认证仍未完成。取消只允许尚未执行的申请；已入队动作拒绝取消，不伪装为撤回已发生的设备效果。Admin 页面不自动执行审批后动作，也不绕过 MFA 或独立批准。
