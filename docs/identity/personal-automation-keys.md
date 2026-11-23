# 个人自动化密钥

个人账户页可以签发一种用途的只读密钥。钱包读者可获取个人余额，预算读者可逐页查询个人预算。用户 Web、Admin、Worker 和托管运行的现有会话各自使用原凭据。

客户端用安全随机数生成 UUIDv4 与 32 字节秘密值，组成 `oa1.<小写 UUIDv4>.<64 位小写十六进制秘密值>`。注册请求上传完整令牌 UTF-8 字节的 SHA-256 小写十六进制摘要。服务端保存摘要、权限、有效期及所属身份。开发自动化可通过下列接口核对结果。[OpenAPI 契约](../../contracts/openapi/account.v1.json)

| 请求 | 凭据和行为 |
|---|---|
| `PUT /api/v1/account/api-keys/{key}` | 有效用户会话、匹配的 Origin 与 CSRF，以及等于 key 的 Idempotency-Key。原键同参重试返回原登记的现有状态；首次提交时起算有效期。 |
| `GET /api/v1/account/api-keys?limit=25&after=…` | 用户会话，按 UUID 对本人密钥元数据分页。 |
| `DELETE /api/v1/account/api-keys/{key}` | 与登记相同的写权限。服务端持久保存撤销决定；撤销先到时也能阻止迟到的同身份登记。 |
| `GET /api/v1/automation/account/wallet` | Bearer 密钥，要求 wallet:read。 |
| `GET /api/v1/automation/account/budgets?limit=50&after=…` | Bearer 密钥，要求 budgets:read。 |

注册受众为 orange-personal-automation-v1，有效期范围为 60 秒至 30 天。读取使用数据库时钟，同时锁定相应密钥、用户及个人账户；返回前复查截止时间。用户停用、个人账户归属变更或账户受限会撤销已有密钥，账户恢复后须重新登记。浏览器登出只结束对应会话，自动化密钥按撤销或到期结束。密钥列表和登记回执只返回元数据。

发行流程中的原始令牌仅在发起客户端可见。响应丢失时可从同一页面重试原登记；已离开页面且未保存密钥时，在账户页按名称与标识撤销后重新创建。服务端保留签发和用户撤销的持久事件，事件载荷仅含用途或撤销原因。
