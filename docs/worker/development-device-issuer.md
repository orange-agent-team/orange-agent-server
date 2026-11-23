# 开发设备证书签发与兑换

一次真实登记依次经过设备生成 CSR、个人或团队管理员授权、独立签发进程准备客户端证书，以及设备兑换证书。签发进程和接收注册请求的 User API 使用独立进程。开发签发进程只访问本地 PostgreSQL，用完即退出。设备端与普通 API 不加载签发私钥。

签发方在当前账户的私有目录下保存配置、CA 证书与 CA 私钥。文件属于当前用户，权限 0600，单硬链接，目录权限 0700。配置字段为 version（1）、environment（development）、region_id、ca_certificate_file、ca_key_file 和 ca_certificate_sha256。路径均为绝对路径。公有 CA 指纹取 DER 的 SHA-256，经可信渠道配置。开发根须自签、用途允许签发证书，并与私钥匹配。网关只接收该设备 CA 的公有部分。网关自身的服务端证书可使用另一套 CA。

```sh
orange-worker-gateway issue-one "$private_issuer_config"
```

用现有开发数据库环境运行上述命令。每次最多准备一张证书，输出本次处理的请求号，没有处理时为 null。程序复查登记用户和所有者状态、团队成员角色、请求签名与截止时间，在同一事务内保存叶证书。证书只允许 TLS 客户端数字签名，有效期最多 24 小时并受 CA 有效期约束。并发签发或进程中断不会登记第二个节点。状态 certificate_prepared 表示已准备证书，尚未激活 Worker 凭据。

兑换时向 `POST /api/v1/worker-enrollments/{request}/redeem` 提交 version（1）和固定 redemption_id。该 UUID 同时作为 Idempotency-Key。Authorization 使用 `Orange-Enrollment`、一个空格和原兑换令牌。令牌为 32 个随机字节的无填充 base64url，授权时提交的是该编码字符串 UTF-8 的 SHA-256。兑换请求不带 Cookie 或 Origin。响应为原请求号、兑换号、节点号、公钥／证书／CA 摘要、证书截止时间与公开 PEM。丢失响应时在原授权期间内按同一身份重试。不同兑换号被拒绝。浏览器会话不能代替兑换令牌。

成功兑换会创建指定所有者的一个离线节点及一条 Worker 凭据。设备使用原私钥、已签发证书及独立固定的网关 CA 建立 mTLS。Owner 撤销请求会同时停用已兑换的凭据；现有控制连接在下一次权威检查时失去权限。注册不会生成 Job、GPU 准入、价格或资源交付。开发 CA 操作、目标设备验证、生产密钥保管、轮换重叠窗口和企业撤销演练分别记录。

一旦登记用户被禁用、所有者被冻结，或组织管理员被降权或移除，数据库会撤销相关未过期授权并停用已兑换凭据。以后恢复账户或管理角色时，需要重新授权新的注册请求。

签发方验证开发 CA 之后，以 CA 证书 DER 摘要和公开 SPKI 摘要登记不可修改的映射。历史证书只保存了 CA 证书摘要。管理员可在自己的授权范围内查看该映射和凭据有效期统计。[管理目录](../engineering/device-issuer-directory.md)
