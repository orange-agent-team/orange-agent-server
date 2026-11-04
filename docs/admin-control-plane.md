# Admin控制面：接口、状态、权限与持久化

日期：2026-11-04。接口草案版本0.1.0，实施NOT_STARTED、运行NOT_RUN。E02维护契约，E07实现身份/审批/领域适配，独立E11 Admin前端消费接口；E10用户Web不拥有管理权限。

## 接口与身份

管理API前缀`/admin/v1`；普通用户API不能使用平台cookie获得特权，Admin API也不接受组织admin角色代替平台身份。OIDC管理员客户端与用户客户端分离，服务端验证issuer/audience/subject及MFA/当前平台授权，返回有效权限与scope。

管理会话使用Secure/HttpOnly/host-only cookie，精确Origin及CSRF校验。设计默认空闲15分钟、绝对8小时；高风险动作执行要求最近15分钟内经服务端验证的step-up。批准有效期默认60分钟，资源/版本/权限改变仍使其失效。数值为待实测配置默认，不是已部署事实。

七种角色与动作权限固定在服务端policy registry；数据库保存管理员实际授权、范围、有效期和撤销版本。模型、工具、MCP和普通服务账号均不能扮演审批人。

## 操作分类与批准政策

查询端点返回按权限过滤的列表/详情；写端点统一创建类型化AdminAction。API不接受任意method/path、SQL或自由脚本。可写动作包括：Run取消、节点排空/隔离/恢复、模型启用/下架、账号限制、凭证撤销、退款、账务冲正、价格发布、已签名发行物灰度/撤销、功能开关、平台角色授予、支持内容授权。

动作参数使用discriminated union，每种action_type对应精确payload schema。创建方不能自选required_approvals；policy_version、风险分类、可执行角色与步骤由服务端计算。默认有界止损可直接进入ready，资金/授权/恢复/扩大上线类动作进入pending_approval。

申请人与批准人必须是不同真实principal。批准记录绑定action_id、规范payload_digest、target版本、policy_version、批准人当前授权与有效期。执行前重新检查这些事实；参数变化建立新action，不能修改原记录继承批准。

## AdminAction状态

pending_approval→ready→executing→succeeded/failed/unknown；审批可rejected，未派发可cancelled/expired。ready表示批准条件满足，不表示业务成功。executing之后取消只停止尚未派发部分，不承诺撤销外部支付/工具/设备效果。

创建时用scope/actor/action_kind/idempotency_key与完整请求摘要去重；同键异参409。执行使用action_id作为稳定业务派发身份，生成领域命令和outbox；重试不换动作/支付键。已执行或未知时返回当前状态和关联case；不重复发退款或冲正。

领域业务的原有守恒/授权/状态约束仍有效：模型启用需认证报告，节点恢复需当前健康/回收证据，价格创建新版本，退款绑定原订单/商户/现金单位，冲正关联原交易和对账case。AdminAction不能用通用success覆盖Job、账本和设备的独立状态。

## 逻辑存储与事务

| 记录 | 必需字段/唯一约束 | 权威 |
|---|---|---|
| AdminPrincipal/Grant | principal、角色/permission、environment/region/tenant scope、有效期、policy version | 平台授权，非租户role |
| AdminSession | opaque session ID、issuer/subject、MFA/step-up时间、expiry、revoked | E07/受控会话存储，浏览器不持供应商token |
| AdminAction | ID、actor、type、scope、ticket/reason、payload digest、target version、policy、state/version/expiry | 同请求业务键唯一，参数不可原地修改 |
| AdminApproval | action、approver、decision、digest/policy、expiry | 一action/approver有效决定，禁止自批 |
| AdminExecution | action、领域命令/业务键、dispatch状态、结果/case | 一action一次受控派发，未知可恢复核对 |
| AuditEvent | ID、actor/session、action、资源/范围、前后版本/结果、原因、时间 | 追加式业务审计，不能由页面删除/重写 |
| SupportGrant | 用户授权引用、resource/data scope、目的、批准/expiry/revocation | 逐次读取鉴权，永不包含凭证明文权限 |

创建/批准/领取执行分别为CAS事务，审计和outbox与状态一起提交；持久化失败时不得先执行效果。外部支付、对象复制、设备命令在事务外；业务完成后按同领域键核对。审计查询分页且限额，导出另需范围与审计，不直接返回全库数据。

## API统一规则

金额为带单位的十进制整数字符串；ID为无语义字符串，时间UTC。列表使用opaque cursor，默认50、最大200；详情默认不含提示词/文件正文/工具输出。所有修改需要Idempotency-Key；版本竞争用If-Match，过期/撤权/状态冲突有独立错误码。

动作响应提供ETag，批准/执行/取消原样回传If-Match。maker_checker表示申请人加一名不同实际主体的批准者，因此required_approvals=1；这不是同人点击两次。除基础actions权限外，还必须具备具体动作permission或approval_permission；[接口索引](admin-api-index.md)与OpenAPI的角色/政策元数据列出完整映射。金额草案字段最多18位，业务层仍按订单可退额度、币种/商户和检查溢出规则再次校验；schema通过不代表可执行。

OpenAPI列出请求/响应/错误、operationId及权限元数据；JSON Schema对action payload拒绝未知字段。`x-permission`和`x-approval-policy`只是文档元数据，实现必须在后端执行，不能因为schema存在就认为已鉴权。

## 配套阶段与未来扩展

ADMIN-W01～06和ADMIN-A01～10对应独立Admin仓库文档；原CP-006仍提供管理CLI/API领域基础，但新增管理会话/页面/审批/审计查询需额外实现。CLI采用同一权限与动作服务，不能绕过双人批准或直接写账本。

G5增加企业身份/审批/保留配置的类型化动作；G6增加区域接管与AllocationGroup操作，只有相应fencing/恢复证据通过才可注册执行policy。当前OpenAPI不提供通用failover、force_idle、set_balance或任意admin命令。

契约源在本仓库contracts；Admin仓库持只读同版本副本，更新需同时记录源摘要与兼容范围。用户API、CloudHost/RunStore、Worker/Broker等其他契约按整体架构编制计划继续展开，不因管理接口草案存在而宣称全系统接口已完成。
