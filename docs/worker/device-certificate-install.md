# 在设备上兑换并安装证书

macOS 和 Linux 的 Worker 使用已生成的设备私钥兑换短期证书。先运行以下命令，并把输出的公开 JSON 交给个人所有者或团队管理员授权。文件包含 CSR 和独立随机令牌的 SHA-256，原令牌留在设备私有目录。获批后由独立开发签发方准备证书。

```sh
orange-worker prepare-enrollment-handoff "$device_directory" "$request_uuid" > "$public_handoff_json"
```

同一目录和请求号可以重复导出相同的公开文件。原令牌位于 `<request_uuid>.exchange-token`，已生成的交接记录与该令牌绑定。[生成请求](device-enrollment-request.md) · [授权及签发](development-device-issuer.md)

将示例配置复制到当前账户的 0700 目录，设置文件权限 0600。填写原请求 UUID、原设备目录、实际开发 User API 地址、可信渠道确认的设备 CA 证书 DER 的 SHA-256 和公有证书路径。单独的 token 文件保存授权时生成的 32 字节随机令牌的无填充 base64url 表示。设备 CA 与 HTTPS 服务端 CA 分别配置。

```sh
orange-worker redeem-enrollment "$private_enrollment_config"
```

开发配置接受本机 HTTP 的数字回环地址，也支持通过专用公有 CA 验证的回环 HTTPS。HTTPS 填写 `api_tls_ca_file`。源地址只含协议、主机和端口。命令直接连接指定源，拒绝重定向。所有输入文件均在当前用户的私有目录中，须为单硬链接、0600 的普通文件。

首次请求先将兑换号、原请求、公钥、源地址、令牌摘要与 CA 摘要同步落盘。超时、服务未完成签发、响应丢失或无效回执时，以同一配置重新运行。设备只接受原请求、原兑换号、自己的公钥和固定签发 CA 对应的有效客户端叶证书。节点号来自该 API 的原兑换回执。实际控制权限由平台网关在 mTLS 连接时核验。设备端保存回执和 PEM 后，输出节点号、证书路径、原私钥路径和截止时间，供已有 Worker 配置的 `node_id`、`certificate`、`private_key` 使用。

`source=gateway_receipt` 表示此次安装使用后端回执。`source=local_receipt` 表示已确认本地文件的一致性与证书有效期。若进程在保存回执后中断，重试可以由已确认的回执恢复 PEM，无需再次提供令牌。过期、证书错配或既有文件冲突会显式报错。撤销仍由网关在设备连接和心跳时执行。Windows 原生设备密钥保管及生产签发环境另行验收。

## 准备设备证书轮换请求

已安装开发证书的设备可以离线申请换用一把新密钥。原私钥保存在本机原目录，替换请求使用新的 v4 UUID。

```sh
orange-worker prepare-certificate-rotation "$private_enrollment_config" "$owner_id" "$new_request_uuid" > "$public_rotation_handoff"
```

Worker 先检查原安装回执、当前有效的客户端证书链和私钥，随后使用新密钥签署 PKCS#10 请求，并用原 Ed25519 设备密钥签署整份轮换意图。签名覆盖所有者、原节点、原请求和证书摘要、签发方、新 CSR 以及兑换令牌摘要。相同私有目录与请求编号支持重复导出。公开文件仅表示设备持有两把密钥，服务端授权、证书激活与旧凭据退出另行处理。
