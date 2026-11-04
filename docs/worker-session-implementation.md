# Worker mTLS 控制会话切片

2026-11-04。当前实现 E07 Worker Gateway 的真实 TLS/gRPC 会话和数据库权威检查。E08 Broker 是独立本机资源许可，控制连接在线不等于获准执行模型。

## 技术与版本

锁定 Tonic/tonic-prost/tonic-prost-build 0.14.9、Prost/prost-types 0.14.4、tokio-stream 0.1.19；Cargo.lock 固定实际依赖和源码校验和。官方发布页对应 [Tonic 0.14.9](https://docs.rs/crate/tonic/0.14.9)、[Prost 0.14.4](https://docs.rs/crate/prost/0.14.4)。本地已下载的 crate 清单中 Tonic 为 MIT，Prost 为 Apache-2.0；完整传递许可/SBOM 仍由候选交付阶段统一审计。

E02 的 `grpc` feature 从服务端唯一 `contracts/proto` 编译 Worker 与 RunStore 类型，要求 Protobuf 编译器 36.2。默认不开此 feature 的 Runtime/桌面消费者无需启用 gRPC。RunStore 目前仅生成类型，服务实现另行推进。打包 E02 时仍需把契约源纳入固定版本发行物，不能依赖开发者机器上的相邻未提交文件。

服务强制 `ServerTlsConfig` 的客户端 CA 和非可选客户端证书，使用 `TlsConnectInfo` 中握手校验后的叶证书 DER 摘要查库，不信任 Hello 自称的身份。接口依据：[Tonic TLS peer metadata](https://docs.rs/tonic/0.14.9/tonic/transport/server/struct.TlsConnectInfo.html)。没有关闭证书校验、可选 mTLS 或开发 token 回退。

## 权威记录与流程

迁移 `0009_worker_control_sessions.sql` 新增：

- `worker_credentials`：证书 SHA-256、node、登记用户、有效区间和撤销状态；由可信 enrollment signer 写入，Hello 不能创建证书映射。
- `worker_sessions`：每 node 唯一当前 Gateway、session、connection epoch、证书、上报能力摘要、45 秒控制会话期限。
- `worker_session_audit`：不可修改/删除的接管和断开记录。

接入时必须先通过真实 mTLS，在 5 秒内发送协议版本 1 的 Hello。服务在 PostgreSQL 事务中校验 node、证书、owner 与登记用户；组织所有权还要求当前 owner/admin 成员资格。凭证、用户、owner 和成员权限持有相应行锁，node 行锁串行化并发接管，然后递增 connection epoch。证书只证明设备密钥，不代替当前组织权限。

Welcome 返回 session、epoch 和控制会话到期时间。HeartbeatAck 只延长这条控制会话，不返回执行 Lease 或 StartGrant。每个心跳和支持的控制操作重新查库，连接空闲时每 2 秒检查一次；数据库不可用、证书撤销/过期、账户停用、owner 受限、成员撤销、旧 epoch 或会话到期都关闭权限。心跳不能复活已过期 session，需重新握手取得新 epoch。

旧 Gateway 的迟到输出或关闭动作不能修改新 session。当前尚未接入 Assignment/ACK/执行 Lease/输出/完成，相关帧返回明确 `UNIMPLEMENTED`，不写 Preparing、不返回 StartGrant、不执行算力任务。未知/空 frame 和重复 Hello 拒绝。后续接执行操作时仍必须在同一业务事务校验当前 session 和任务 fencing，不能只依赖建立连接时的鉴权。

全局每 Gateway 最多 128 条控制流，每连接最多 8 条并发流，单消息最多 64 KiB，输出队列最多 8 项。慢读者、数据库等待和 TLS/Hello 均有上限。关闭 Gateway 会通知活动流退出；若无法写断开记录，已有会话仍按数据库期限失效，不能假设内存退出已经删除权威记录。

## 开发入口

`orange-worker-gateway` 属于 E07，不新增核心工程。必须设置 `ORANGE_ENV=development`；默认 `127.0.0.1:43104`，非 loopback 地址拒绝。

配置：`ORANGE_WORKER_GATEWAY_CONFIG` 指向 JSON，字段为 `gateway_instance_id`、`server_certificate_file`、`server_key_file`、`client_ca_file`。配置和私钥为 0600、父目录为 0700；拒绝符号链接/多硬链接秘密文件和超过 64 KiB 的输入。`DATABASE_URL` 通过独立秘密环境提供，日志不输出连接串或 PEM。运行 `cargo run -p orange-control-plane --bin orange-worker-gateway --locked`。

服务启动不会创建、签发或自动信任任意客户端证书。当前公开 enrollment/CSR、一次性 token、证书轮换、用户退出与设备贡献撤销的产品接口仍未完成。不能直接开放公网或把手工插入证书映射当作正式注册流程。

## 验证证据

E08 的 `services/worker/src/control.rs` 已实现实际客户端库：强制HTTPS/mTLS、限定帧大小、验证Welcome/HeartbeatAck绑定node/session/epoch，并从请求发送时的单调时间计算44秒本地停止上限。它不依赖客户端墙钟，不把控制在线状态转换成任务执行Lease。网络结果不明、撤权、旧世代、未知执行消息或心跳Future被丢弃后，本连接不能继续复用，必须重新完成认证握手。当前没有自动重试执行命令或伪造ACK。

`python3 scripts/check-worker-session.py` 生成临时本地 CA 和独立客户端证书，仅用于测试；不修改系统信任库。真实 loopback TLS/HTTP2/gRPC 与专用 PostgreSQL 测试覆盖：

- 正确证书绑定 node，心跳只产生 HeartbeatAck；未知任务执行能力明确失败。
- 无证书、错误 CA、未登记证书及冒充其他 node 全部失败。
- 两个 Gateway 顺序/同时争同一 node，只有最新 epoch 可续控制会话，旧关闭不能下线新连接。
- 已连接证书撤销、账户停用、owner 受限和组织成员撤销即时失去后续权限。
- 数据库连接池实际关闭后不使用旧内存权限；协议不匹配与到期会话不能靠心跳恢复。
- 实际E08客户端拒绝明文端点，撤权和旧epoch使其失效；真实网络心跳Future被中断后仍保持失效，不能靠取消Future延长权限。

测试中的 CA、设备登记记录和账户是明确夹具；mTLS 握手、网络传输与数据库不是替身。实际 Windows Worker、公共注册、设备密钥保护、完整任务命令投递、真实 GPU 和生产跨域控制未因此获得认证。
