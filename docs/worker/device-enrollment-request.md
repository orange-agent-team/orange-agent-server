# 在设备上生成注册请求

macOS 与 Linux 的 `orange-worker prepare-enrollment` 在设备所有者指定的绝对路径内生成 Ed25519 私钥。目录必须已存在、属于当前有效用户，权限为 0700。Worker 使用单链接 0600 文件保存 PKCS#8 私钥。请求使用标准 PKCS#10 格式，公钥摘要采用 DER SubjectPublicKeyInfo 的 SHA-256，CSR 摘要采用整个 DER 请求的 SHA-256。[PKCS#10](https://www.rfc-editor.org/rfc/rfc2986.html) · [Ed25519 公钥编码](https://www.rfc-editor.org/rfc/rfc8410.html#section-4)

```sh
orange-worker prepare-enrollment "$private_key_directory" "$request_uuid" > "$public_request_file"
```

为一次注册生成一个规范小写 v4 UUID。重试时使用同一目录和 UUID，程序重新打开原有私钥、派生相同公钥并生成相同请求。创建过程锁定同一目录，并在发布私钥和目录项后分别同步落盘。进程在发布前中断时，下次调用核对暂存文件并恢复。已经发布但损坏的私钥会阻止该请求继续；原文件保留供所有者处理。程序拒绝不合权限的目录或文件、末级目录或私钥的符号链接以及多硬链接私钥。输出 JSON 只含版本、用途、请求号、两个公开摘要与 CSR。

请求主题为 `CN=orange-worker-<request_uuid>`，请求数字签名和 TLS 客户端用途。设备注册权、个人或团队归属、地区、证书期限、轮换和撤销均需由登记服务单独核验。这个本机命令只准备请求。原生 Windows 密钥存储和目标平台验证仍各自记录。[公共请求契约](../../contracts/json-schema/worker-certificate-request.v1.json)

本机回归分别运行八个 Worker 进程和独立 OpenSSL 验证器。测试签发方仅使用公开 CSR 签发一张临时客户端叶证书，通过真实 TLS 双向握手核对服务端收到的证书 DER，同时验证同一证书不能作为 TLS 服务端。平台登记服务另行验收。

Rust 依赖 rcgen 0.14.10 调用 ring 生成设备密钥并制作 CSR，依赖版本及制品校验和保存在 Cargo.lock。签发方需独立验证请求签名和叶证书用途。[rcgen 证书请求接口](https://docs.rs/rcgen/0.14.10/rcgen/struct.CertificateParams.html#method.serialize_request)

私钥使用兼容 OpenSSL 3.0 的 PKCS#8 v1 编码。重开旧版 v2 文件时，Worker 在目录锁内核对原公钥和新编码的公钥，只有一致才同步临时文件并原子替换编码。相同请求继续产生相同 CSR。
