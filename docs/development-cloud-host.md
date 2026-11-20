# 本地托管工作区 Host

开发 Host 是服务端仓库的独立可执行入口 orange-cloud-host-node。它只持有平台 Host 客户端凭据及本机分配日志，数据库和用户 OIDC 凭据由控制面持有。当前负载会把 Run 指定的不可变纯文本版本写入专属 gVisor 工作区并保持就绪。RunStore 模型步骤、用户工具批准和推理结算继续由后续切片接入；就绪凭据只证明工作区准备完成。

## 启动顺序

1. 在本机配置用户 API 与专用对象根目录，准备独立平台 Host CA、gateway 证书和 clientAuth 叶证书。gateway 的服务器名称为 localhost。将签名材料保存在私有开发目录中。
2. 启动 orange-cloud-host-gateway。设置 ORANGE_ENV=development、DATABASE_URL、ORANGE_CLOUD_HOST_TLS_DIR，并将 ORANGE_CLOUD_HOST_ARTIFACT_ROOT 指向用户 API 已注册的同一对象根。网关以只读方式打开对象存储。未配置读端时不接受输入协议 v1 的新分配。
3. 按云 Run 准入说明运行 scripts/admit-local-cloud-host.py。它会实测固定的 gVisor Linux ARM64 配置，返回 host_instance_id 与 attestation_digest。默认缓存位于 .local/gvisor-download。gVisor 运行在项目的独立 Colima 虚拟机内，默认 Docker 上下文与 Keycloak 继续保留。
4. 将下方配置中的示例值替换为本机实际绝对路径和准入输出。journal_dir 的父目录必须已存在；Host 创建权限为 0700 的私有目录，持有独占锁。driver_root 指向安装的服务端源码树，其后端脚本与负载源码必须与二进制中固定的内容一致。
5. 设置 ORANGE_ENV=development 和 ORANGE_CLOUD_HOST_CONFIG，然后运行 cargo run --locked -p orange-control-plane --bin orange-cloud-host-node。用户可以在 /agent 查看容量、选择预算并发起 Run。

```json
{
  "endpoint": "https://127.0.0.1:43107",
  "host_instance_id": "dev-host-01",
  "attestation_digest": "<实际准入摘要>",
  "ca": "/private/development/pki/platform-host-ca.pem",
  "certificate": "/private/development/pki/host.pem",
  "private_key": "/private/development/pki/host.key",
  "journal_dir": "/private/development/host-state",
  "driver_root": "/private/development/orange-agent-server",
  "archive": "/private/development/orange-agent-server/.local/gvisor-download/gvisor-aarch64.tar.bz2",
  "python": "/usr/bin/python3"
}
```

## 一次分配的证据

Host 先持久保存原领取命令。网关在输入读取前后核对 mTLS 身份、工作区世代、租约截止时间、用户、当前预算以及原始文件版本。私有对象读端校验全部内容的 SHA-256 和 UTF-8 后交付，日志仅保存引用和摘要。

Host 在虚拟机中先登记分配标识，持久记录已开始派发后才启动 OCI 后端。固定 Rust 负载以 UID 65534 将输入写入私有 tmpfs，回读核对相同字节后到达准备屏障。监督器核对运行中的 OCI 实例及其真实内核 cgroup，产生材料化凭据。服务端将它绑定到同一分配、认证配置、不可变输入和具体输入投递凭据，然后在事务中将工作区转为 ready。用户 Run 的状态仍是 queued。

工作区只在有效租约内持有输入。用户取消、权限变化、租约丢失或 Host 停机时，控制面先撤销旧执行权。监督器关闭固定负载、验证原沙箱和 cgroup 消失，移除该分配的专属文件，再持久保存终态凭据。网关收到针对原分配的确认才释放容量。运行中 Host 进程退出后，新进程按私有日志处理原分配，不能重新启动同一工作区或凭路径缺失推断回收。

本机可运行 scripts/check-cloud-run-admission.py，并传入 --qualify-host、--managed-host 以及独立数据库、用户身份和 Web 仓库参数。验收应同时核对控制面回执、Host 本机日志与 Linux 虚拟机证据。此入口服务于本机开发，不提供生产集群部署认证。

黑盒验收会对真实分配分别发出用户取消、终止并重新启动原 Host 进程，以及删除原始输入。最后一种情况下下一次授权检查会阻断旧分配；Host 随后清理 tmpfs、沙箱和 cgroup。另一个真实浏览器运行会单独经过 ready、draining、deleted 三种工作区状态。每种情况保存控制面和 Linux 后端针对同一 allocation_id 的证据，不通过重新创建容器补齐丢失的回执。
