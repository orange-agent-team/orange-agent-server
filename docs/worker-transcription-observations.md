# 当前节点会话的转写配置观测

WorkerGateway 提供 ReportTranscriptionObservation RPC。发送方需要有效设备证书及当前 execution_control_version=2 会话。请求包含经本机实际样本测量的规范化 runtime 描述、已审核模型引用和固定公开录音的计量事实。网关独立重算执行配置摘要，并核对会话、证书、节点在线状态及 reviewed_execution_profiles 中当前启用的记录。租约、输入访问、价格和账本沿各自独立授权路径建立。

节点自己的私有开发配置可增加 transcription_observation 对象。execution_profile_id 是已由平台独立审核的配置编号。private_probe_file 是只在本机读取的绝对路径；配置格式见[Worker 运行时资格](worker-transcription-qualification.md)。

```json
{
  "transcription_observation": {
    "execution_profile_id": "reviewed-local-asr",
    "private_probe_file": "/absolute/private/probe.json"
  }
}
```

启用后，前台 `orange-worker` 在建立控制会话前运行本地探测，待启动恢复工作完成再上报。每个会话与配置生成固定请求键，成功时输出 transcription_profile_observed。审核未启用或旧网关不支持时输出 transcription_profile_not_admitted，本机资格测试失败时输出 transcription_probe_failed。已有文本节点仍可处理其原有能力。配置缺省时不会发出上报 RPC。

迁移 0051 保存不可变的设备、会话 epoch、配置、公开录音和计算精度事实。相同请求返回已有回执，相同键替换配置、同一 epoch 重复创建另一回执都会拒绝。换会话后须使用新 epoch。撤销审核或设备凭证后旧回执也不能通过该 RPC 重新取得。历史记录不构成当前可调度资源。设备自报加 mTLS 不等同于远程硬件证明。

`python3 scripts/check-transcription-observation.py` 在独立数据库与临时 mTLS 上验收。测试使用经真实录音观测的 CPU profile，并显式建立独立的开发审核夹具。覆盖并发重复、配置替换、描述符重复字段、其他设备证书、过期 epoch、撤销和数据库不可变性。前台 Worker 实际运行默认、已获审核、审核撤销三个配置状态。该入口不会创建 Job、报价或账本交易。[协议定义](../contracts/proto/worker.proto)

兼容门槛在另一个空 PostgreSQL 数据库上按原 SQLx 校验和依次应用 1 至 50 号迁移，填入已有审核配置并维持一条旧协议 Worker 控制会话。应用 0051 后再运行一次迁移，原证书、session_id、epoch 和心跳仍有效；既有审核配置保留，新增观测表为空。该验收验证真实旧结构与真实在线连接，未使用历史生产数据。

### 历史网关在线升级验收

版本 50 的数据库由固定提交 `b6a542f7414851b5336adb86bb9a7fe6e12416c8` 的网关建立原控制会话。测试验证握手和心跳时数据库仍为 50，并保留旧会话，在同一个临时数据库上迁移两次。迁移后的心跳仍使用原设备、会话编号和 epoch，历史文本能力保留，转写观测表保持为空。

版本 51 的输入协议同样使用其发布提交 `ef97803d5db5f5f9d8324a81c80bf853069bfe3f`。先让旧客户端在原网关实际轮询，然后迁移；新网关通过新的 Hello 协商音频能力并接管控制，旧客户端重新连接时只能获得自身声明的能力。

`python3.12 scripts/check-historical-worker-upgrade.py --schema 50` 与 `--schema 51` 可单独运行。这两个检查使用点名版本的离线编译产物、私有临时 TLS 和一次性数据库。完整的转写观测与远程输入验收还会实际加载审核模型；迁移专项记录只覆盖协议和数据库。
