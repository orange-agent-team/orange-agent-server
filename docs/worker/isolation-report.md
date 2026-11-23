# 贡献者隔离观测回执

当前 Unix 前台 Worker 可在现有 mTLS 控制会话中提交一次隔离诊断。网关从 TLS 证书和当前 session/epoch 取得节点身份。报告只记录进程观测，调度器不从这些记录派发 GPU、工作区、资金或 Wasm 权限。

Windows 预检包可提供同一版探针与策略。原生 Windows 前台上传和 GPU 驱动流程需要另行实测。

在已经配置好的私有 Worker 配置文件内可增加以下字段。缺省时保持原有 CPU 流程。选择分享前，应使用已审核发布中的 Linux 静态探针，把 JSON 输出保存在设备所有者的 0700 私有目录，文件权限使用 0600。受限目标上应同时采集 Windows 驱动、GPU 模型执行和回收的独立证据。

```json
{
  "isolation_diagnostics": {
    "share_with_platform": true,
    "request_id": "first-private-observation",
    "private_probe_file": "/private/worker-evidence/process.json",
    "oci_policy_sha256": "2fa2e7a0b4597af9a30dfedbfec8c1a3500857f30d74d888aec4841d94d34fdb",
    "linux_probe_sha256": "2d4afe4294922d1c2b4abb16ca4ab1ecf6cf2c038000d86ce93eca6c00f90544"
  }
}
```

Worker 只读取当前用户拥有的单链接常规文件，最多 4096 字节。报告带上该文件的修改时间并在读取后复核文件状态。这个时间和进程数据的来源均为节点自报。首次提交在锁定当前节点和会话后检查时间，接受最近 300 秒的样本，允许最多 30 秒时钟领先。回执中的摘要绑定请求标识、代次、时间、策略、探针和完整内容。重试应保留原请求标识、内容与文件时间。

`WorkerGateway.ReportIsolationObservation` 是 v1 的可选 unary RPC。现有 Hello 与 Welcome 字段保留原有编号和语义。未实现该方法的网关返回 UNIMPLEMENTED。跨节点证书不能借用会话；替换会话、撤销权利或到期会使旧请求失效。相同请求的并发重试取得同一回执。当前会话内同键异参返回 ALREADY_EXISTS。每节点每小时至多新增 24 条，已接受的回执在当前授权下可以重查。

`incomplete` 表示观测缺失或控制条件不匹配。`independent_review_required` 表示节点上报了一组内部一致的进程与有界对照结果，仍需平台核验目标运行。两种回执的 supply_authorized 和 wasm_enabled 均为 false。前台事件只显示结果码和已确认的观测编号。首次提交必须携带 Welcome 或最新心跳应答中的策略代次。网关在节点锁内核对并记录该代次。内部视图只提供仍在时限内、节点在线、当前会话与策略代次一致且凭据及归属权限有效的最新报告编号和结果。旧回执重查不会延长有效期。普通诊断约 30 天后由后台分批清理。

[固定发布声明](../../config/worker-isolation-release.v1.json) · [gRPC 契约](../../contracts/proto/worker.proto) · [数据库迁移](../../services/control-plane/migrations/0078_worker_isolation_observations.sql)
