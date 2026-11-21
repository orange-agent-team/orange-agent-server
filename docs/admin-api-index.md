# Admin API接口索引

契约 0.1.11 定义 48 个路径、55 个 HTTP 操作和 20 种管理动作。下方保留初版索引，并补入对账事项变更。登录、step-up、会话撤销上下文及范围权限查询参见当前契约。[OpenAPI](../contracts/openapi/admin.v1.json) · [JSON Schema](../contracts/json-schema/admin.schema.json)

## 操作清单

所有列表按环境/授权范围过滤、默认50最大200；详情也做当前scope检查。修改请求须Idempotency-Key和管理cookie/CSRF，批准/execute/cancel另带服务器返回的ETag原值作为If-Match。状态响应不表示外部效果或GPU清理已经完成。

| HTTP | 路径 | operationId | 请求schema | 成功响应 | 基础权限 |
|---|---|---|---|---|---|
| `GET` | `/admin/v1/session` | `getAdminSession` | `无body` | `AdminSession` | `session.read` |
| `DELETE` | `/admin/v1/session` | `revokeAdminSession` | `无body` | `204` | `session.revoke` |
| `GET` | `/admin/v1/overview` | `getAdminOverview` | `无body` | `Overview` | `overview.read` |
| `GET` | `/admin/v1/users` | `listUser` | `无body` | `UserList` | `users.read` |
| `GET` | `/admin/v1/users/{user_id}` | `getUser` | `无body` | `User` | `users.read` |
| `GET` | `/admin/v1/organizations` | `listOrganization` | `无body` | `OrganizationList` | `organizations.read` |
| `GET` | `/admin/v1/organizations/{organization_id}` | `getOrganization` | `无body` | `Organization` | `organizations.read` |
| `GET` | `/admin/v1/runs` | `listRun` | `无body` | `RunList` | `runs.read` |
| `GET` | `/admin/v1/runs/{run_id}` | `getRun` | `无body` | `Run` | `runs.read` |
| `GET` | `/admin/v1/workspaces` | `listWorkspace` | `无body` | `WorkspaceList` | `workspaces.read` |
| `GET` | `/admin/v1/workspaces/{workspace_id}` | `getWorkspace` | `无body` | `Workspace` | `workspaces.read` |
| `GET` | `/admin/v1/nodes` | `listNode` | `无body` | `NodeList` | `nodes.read` |
| `GET` | `/admin/v1/nodes/{node_id}` | `getNode` | `无body` | `Node` | `nodes.read` |
| `GET` | `/admin/v1/models` | `listModelVersion` | `无body` | `ModelVersionList` | `models.read` |
| `GET` | `/admin/v1/models/{model_version_id}` | `getModelVersion` | `无body` | `ModelVersion` | `models.read` |
| `GET` | `/admin/v1/payments` | `listPayment` | `无body` | `PaymentList` | `payments.read` |
| `GET` | `/admin/v1/payments/{payment_order_id}` | `getPayment` | `无body` | `Payment` | `payments.read` |
| `GET` | `/admin/v1/ledger/transactions` | `listLedgerTransaction` | `无body` | `LedgerTransactionList` | `ledger.read` |
| `GET` | `/admin/v1/ledger/transactions/{transaction_id}` | `getLedgerTransaction` | `无body` | `LedgerTransaction` | `ledger.read` |
| `GET` | `/admin/v1/reconciliation-cases` | `listReconciliationCase` | `无body` | `ReconciliationCaseList` | `cases.read` |
| `GET` | `/admin/v1/reconciliation-cases/{case_id}` | `getReconciliationCase` | `无body` | `ReconciliationCase` | `cases.read` |
| GET | /admin/v1/reconciliation-cases/{case_id}/activity | listReconciliationCaseChanges | 无body | ReconciliationCaseActivity | cases.read |
| `GET` | `/admin/v1/risk-cases` | `listRiskCase` | `无body` | `RiskCaseList` | `risk.read` |
| `GET` | `/admin/v1/risk-cases/{case_id}` | `getRiskCase` | `无body` | `RiskCase` | `risk.read` |
| `GET` | `/admin/v1/releases` | `listRelease` | `无body` | `ReleaseList` | `releases.read` |
| `GET` | `/admin/v1/releases/{release_manifest_id}` | `getRelease` | `无body` | `Release` | `releases.read` |
| `GET` | `/admin/v1/feature-flags` | `listFeatureFlag` | `无body` | `FeatureFlagList` | `features.read` |
| `GET` | `/admin/v1/feature-flags/{feature_id}` | `getFeatureFlag` | `无body` | `FeatureFlag` | `features.read` |
| `GET` | `/admin/v1/audit-events` | `listAuditEvent` | `无body` | `AuditEventList` | `audit.read` |
| `GET` | `/admin/v1/audit-events/{event_id}` | `getAuditEvent` | `无body` | `AuditEvent` | `audit.read` |
| `GET` | `/admin/v1/actions` | `listAdminActions` | `无body` | `AdminActionList` | `actions.read` |
| `POST` | `/admin/v1/actions` | `createAdminAction` | `AdminActionCreate` | `AdminAction` | `actions.create` |
| `GET` | `/admin/v1/actions/{action_id}` | `getAdminAction` | `无body` | `AdminAction` | `actions.read` |
| `POST` | `/admin/v1/actions/{action_id}/approvals` | `decideAdminAction` | `ApprovalCreate` | `AdminAction` | `actions.approve` |
| `POST` | `/admin/v1/actions/{action_id}/execute` | `executeAdminAction` | `ExecuteRequest` | `AdminAction` | `actions.execute` |
| `POST` | `/admin/v1/actions/{action_id}/cancel` | `cancelAdminAction` | `CancelRequest` | `AdminAction` | `actions.cancel` |
| `GET` | `/admin/v1/support-grants` | `listSupportGrants` | `无body` | `SupportGrantList` | `support.read` |
| `POST` | `/admin/v1/support-content` | `readGrantedSupportContent` | `SupportRead` | `SupportContent` | `support.content.read` |
| `GET` | `/admin/v1/refunds` | `listRefund` | `无body` | `RefundList` | `refunds.read` |
| `GET` | `/admin/v1/refunds/{refund_id}` | `getRefund` | `无body` | `Refund` | `refunds.read` |

## 类型化动作与额外权限

创建/执行除了actions.create或actions.execute，还必须有下表动作权限；批准还需actions.approve及对应approval_permission。环境/地区/租户/资源范围、当前角色、MFA/step-up、申请人与批准人不同均由后端验证。

| action_type | 动作权限 | 批准政策 | 批准权限 |
|---|---|---|---|
| run.cancel | runs.cancel | single_operator | 不需要独立批准，但仍需实时授权/审计 |
| node.drain | nodes.drain | single_operator | 不需要独立批准，但仍需实时授权/审计 |
| node.quarantine | nodes.quarantine | single_operator | 不需要独立批准，但仍需实时授权/审计 |
| node.resume | nodes.resume | maker_checker | nodes.resume.approve |
| model.activate | models.activate | maker_checker | models.activate.approve |
| model.disable | models.disable | single_operator | 不需要独立批准，但仍需实时授权/审计 |
| account.restrict | accounts.restrict | single_operator | 不需要独立批准，但仍需实时授权/审计 |
| credential.revoke | credentials.revoke | single_operator | 不需要独立批准，但仍需实时授权/审计 |
| payment.refund | payments.refund | maker_checker | payments.refund.approve |
| case.update | cases.edit 或 cases.close | 分派、备注为 single_operator，关闭为 maker_checker | cases.close.approve |
| ledger.reverse | ledger.reverse | maker_checker | ledger.reverse.approve |
| pricing.publish | pricing.publish | maker_checker | pricing.publish.approve |
| release.rollout | releases.rollout | maker_checker | releases.rollout.approve |
| release.revoke | releases.revoke | single_operator | 不需要独立批准，但仍需实时授权/审计 |
| feature.disable | features.disable | single_operator | 不需要独立批准，但仍需实时授权/审计 |
| feature.enable | features.enable | maker_checker | features.enable.approve |
| admin.role_grant | admins.grant | maker_checker | admins.grant.approve |
| support_access.grant | support.grant | maker_checker | support.grant.approve |

maker_checker为申请人加至少一名不同实际主体的批准者，响应required_approvals=1；不是同一人点两次，也不是需要另外两人。single_operator仅适用于列出的有界止损。审批不放宽原领域前置条件。

OIDC登录/回调的完整协议、用户API和云Host等另按整体接口计划编制；本索引不宣称全部系统接口已经完成。当前仅有文档/请求数据，没有可调用服务。

返回[管理控制面说明](admin-control-plane.md)。


开发文本定价使用 GET /admin/v1/price-drafts、GET /admin/v1/price-drafts/{price_id} 和 POST /admin/v1/price-drafts。草稿限定 dev 的租户或平台范围。费率使用整数字符串表示每千计量单位的微积分，生效时间使用 UTC 整秒。发布沿用 pricing.publish 的独立审批和固定摘要。

支持操作通过 GET /admin/v1/support-authorizations/{id} 核对用户授权。support_access.grant 需要客服申请、安全人员独立批准及执行。support_access.revoke 可由拥有相应租户权限的客服或安全管理员执行。授权记录通过 GET /admin/v1/support-grants 和 GET /admin/v1/support-grants/{id} 查询。正文按次通过 POST /admin/v1/support-content 核对。

平台角色绑定通过 GET /admin/v1/principals/{principal}/role-bindings/{role} 核对。授予与撤销均以目标版本、当前作用域、申请人与独立批准人为依据。目标人员已有管理主体标识，不能充当本次授予或撤销的申请人、独立批准人或执行人。

### 开发转写定价

`GET /admin/v1/transcription-price-drafts` 列出当前管理范围中的转写草稿。`POST` 在同一路径创建草稿，所需权限为 `pricing.draft`。请求包括管理范围、已审核观测编号、每分钟微积分费率和平台费率基点。源音频时长以毫秒计量，整单向上取整。`GET /admin/v1/transcription-price-drafts/{price_id}` 返回节点、提供方、执行配置、不可变条款及审核摘要，读取需要 `pricing.read`。

草稿作者通过既有的 `pricing.publish` 动作提交审核。审批和执行均重读范围权限，只有原审核记录仍然匹配时才发布。指定未来时间的版本在到期时重新核对审批人的权限。已有启用价格时，新草稿不会暗中覆盖旧价格。所有金额均为本地开发积分。实际可报价还要求有效的用户输入授权、预算及当前 Worker 会话能力。[Admin 契约](../contracts/openapi/admin.v1.json)

`GET /admin/v1/test-refunds/{case_id}` 在当前 dev 平台或租户范围内按 `refunds.read` 返回原单、所持微积分、原币最小单位和条款摘要。既有 `payment.refund` 支持开发测试订单的独立财务审批。执行的业务引用是本地退款编号；商户实际结果由退款状态查询返回。[开发退款规则](test-checkout.md)

### 测试商户日报

开发环境通过 `GET /admin/v1/test-statements` 分页列出扫描，`GET /admin/v1/test-statements/{run_id}` 返回封存报告和对应差异。二者仅对平台范围开放。使用 `GET /admin/v1/test-statement-issues/{issue_key}` 查看差异的带摘要观测，选填 run_id，省略时取该范围最近一次封存记录。读取需当前会话同时拥有 payments.read、ledger.read、cases.read。租户只能读归属于其账户的差异；每次读取写入独立的 Admin 审计。金额与微积分分别采用十进制整数字符串。[范围及复验](test-checkout.md)

Admin 0.1.15 在 case.update 中增加明确的 retry_metering 变体。请求只接受事项编号和原计量事件编号。制单与独立批准分别要求 metering.retry、metering.retry.approve，并校验相应的 actions 权限。GET 对账事项活动可返回 metering，其中列出服务端事件身份、处理轮次、尝试次数、政策、到期时间和最近失败类别。查看范围始终绑定事项所属账户。
