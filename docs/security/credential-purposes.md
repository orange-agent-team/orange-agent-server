# 凭据用途与责任

| 凭据 | 保管方 | 接受方与授权条件 | 撤销或替换 |
|---|---|---|---|
| 用户会话与 CSRF | 用户浏览器 / 用户身份服务 | 用户 API 校验用户 cookie、当前用户和所有者权限，写入另查来源及 CSRF | 注销、用户禁用、空闲与绝对时限 |
| Admin 会话与 step-up | 管理员浏览器 / 管理身份服务 | Admin API 独立 cookie、角色、范围、MFA 和双人审批 | 注销、授权到期或撤销、管理员禁用 |
| 公共 Worker 叶证书私钥 | 节点所有者，推理引擎无读取权 | Worker Gateway 校验 mTLS，按证书指纹绑定节点和登记用户，再核对当前权限、会话代次与租约 | 登记项停用、过期或失去当前所有者权限 |
| 托管 Host 叶证书私钥 | 平台工作区宿主 | 平台 Host Gateway 的客户端 CA 与独立 Host 登记、区域、工作区租约 | 禁用 Host、证书过期或收回租约 |
| 设备或 Host CA 私钥 | 各自的签发服务 | 为指定用途签发短期叶证书，在线网关只需对应公有信任根 | 撤销签发权并替换信任根；生产签发保管待独立验证 |
| 开发发行私钥 | 独立开发发行签名方 | 离线核验器按可信渠道提供的公钥指纹、仓库、源码提交、目标和归档摘要验签 | 更换独立固定的公钥并重新核验；生产签名、撤回和公证另有门禁 |
| 商户测试 API 密钥 | 支付适配器 | 仅服务端沙箱商户操作，密钥限定测试前缀 | 商户轮换和作废 |
| 支付回调 HMAC 密钥 | 服务端支付回调 / 商户 | 对时间戳与原始正文字节验签，再核对商户订单、金额和幂等 | 商户更换回调密钥，拒绝超窗或不符的签名 |
| 数据恢复清单私钥 | 独立恢复授权方 | 恢复程序检查外部可信摘要、RSA-PSS 签名和删除记录 | 从独立备份控制面更换恢复信任来源 |

## 数据库升级

迁移 79 为过去的叶证书指纹建立用途登记，包含用途、节点或 Host 标识，以及公共 Worker 的登记用户。正常单一用途凭据保留原有效期。发现同一个指纹同时存在于两个登记表时，迁移在同一事务中停用双方，结束已有 Worker 控制会话，关闭托管 Host 接单，并保留冲突指纹。运维须分别签发新叶证书；已退役的托管 Host 应分配新的宿主标识后再登记。历史 Job 和资金记录保留原状。可在授权的数据库运维会话中按用途检查冲突数量。

```sql
SELECT purpose, count(*) FROM execution_certificate_purposes GROUP BY purpose;
```

新登记通过 PostgreSQL 唯一索引串行取得用途。并发冲突者的整笔登记回滚。删除某侧登记后，指纹仍绑定原用途和主体。该约束的身份键是叶证书 DER 的 SHA-256；签发方还须确保设备和两个签发域使用独立私钥。它不能凭指纹发现用同一私钥签发的不同证书。实际 CA 保管和企业签发轮换须在目标部署验收。

## 离线验证开发包

安装者从独立可信渠道取得验证脚本、公钥指纹、预期仓库、提交及构建目标。外部签名方对描述文件的完整 UTF-8 字节生成 Ed25519 分离签名。描述必须恰好包含 purpose、repository、source、target、key_spki_sha256、archive_sha256 六项。用途固定为 `orange.development-release.v1`。一般安装包内的文件哈希、代码签名、公证和发布授权各自记录。开发验收使用临时密钥。正式发行私钥保持在发布方。[Ed25519 公钥格式](https://www.rfc-editor.org/rfc/rfc8410.html#section-4)

```sh
python3.12 -I verify-candidate-signature.py \
  --archive "$candidate_archive" --descriptor "$signed_descriptor" --signature "$detached_signature" \
  --public-key "$release_public_key" --key-sha256 "$trusted_release_key_digest" \
  --repository orange-agent-server --source "$expected_commit" --target "$expected_target"
```
