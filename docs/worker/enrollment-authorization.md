# 授权 Worker 注册请求

节点所有者从设备上的 `orange-worker prepare-enrollment` 取得公开请求。用户端需另外以密码学安全随机源生成独立的兑换令牌，保管原令牌，只向会话接口提交其 SHA-256 十六进制摘要。客户端不应从公开 CSR、请求 UUID 或公钥派生令牌。服务端保存公开 CSR 的 DER、公钥和请求摘要、令牌摘要、发起用户、所属个人或组织与截止时间。签发 CA 私钥和设备私钥均不参与这个 HTTP 调用。

用户使用现有会话向 `/api/v1/owners/{owner}/worker-enrollments/{request}` 发起 PUT。Origin、CSRF 和唯一 Idempotency-Key 均须有效，幂等键等于设备请求 UUID。个人账户只能操作自己的请求。团队每次读取、授权、撤销都受当前 owner/admin 成员身份约束。

服务端验证 PKCS#10 签名、Ed25519 公钥、指定请求号的唯一 CN、数字签名及 TLS 客户端用途。附带 SAN、CA、服务端用途、额外属性、错误摘要、损坏签名或请求号不一致均被拒绝。同键同参重复提交返回原编号与原截止时刻。同键异参返回冲突。每个所有者每小时可新增 24 项，每项自首次提交起有效 10 分钟。GET 只返回元数据。DELETE 撤销授权，重复调用可读取原撤销结果。

本接口只建立签发前的用户授权。在提交、撤销和重试时均不创建节点、登记证书、启用 GPU、分配租约或操作资金。到期授权可查询其过期状态。到期满 30 天的授权按批清理。平台签发器、一次性兑换、设备证书轮换和安装包联调另有验收。[HTTP 契约](../../contracts/openapi/worker-enrollment.v1.json) · [设备请求](device-enrollment-request.md)
