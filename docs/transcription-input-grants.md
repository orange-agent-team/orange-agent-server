# 远程转写的音频输入授权

用户先预检已经上传的音频，再用已获知的 observation_id 查询当前受审节点的地区、节点编号和执行配置摘要。查询需要登录，仅返回该条观测。提供者的证书、会话凭证、主机路径和录音内容保留在各自的信任边界内。节点重连、凭证或审核被撤销后，原观测不再匹配。

创建授权的请求绑定当前用户、所有者、预算、唯一 invocation_id、不可变音频版本、服务端预检回执、目标节点及地区、执行配置、可选语言和有效时长。最长 900 秒。调用方必须明确设置 allow_remote_transcription，Idempotency-Key 等于 grant_id。服务端复算实际音频时长、采样数、原始与标准化摘要，并保存每个 32 KiB 分块的摘要。创建授权本身不派发 Job，也不会改变账本。

| 接口 | 用途 |
| --- | --- |
| GET /api/v1/transcription-targets/{id} | 根据观测编号确认当前目标 |
| POST /api/v1/owners/{owner}/transcription-input-grants | 用户明确授权一份音频和一次调用 |
| GET /api/v1/owners/{owner}/transcription-input-grants/{id} | 查询本人授权，活动授权会复核源、预算和节点 |
| DELETE /api/v1/owners/{owner}/transcription-input-grants/{id} | 请求撤回。先到的撤回可以阻止同一签发者随后到达的创建请求 |
| WorkerGateway.ReadTranscriptionInput | 有效设备会话凭已启动的专属转写 Job 租约，按对齐偏移读取一块已核验音频 |

Worker 每次读取需满足现有设备身份、epoch、节点策略版本、租约栅栏与期限、Job、报价执行计划，以及当前输入授权。读取实际对象后，交付前再次检查源 ACL、目标、预算、授权和租约。响应中的 eof、偏移、大小、版本和摘要用于校验分块与完整组装。撤回阻止之后的交付，已发出的字节受原接收方的数据使用约定约束。

`python3 scripts/check-transcription-input-grants.py` 使用真实 PostgreSQL、私有对象存储、Whisper 录音、OIDC 发现、用户 HTTP 路由与 mTLS 网关。用户会话、独立审核记录、已排队的零金额开发 Job 和本机设备探测是显式夹具。正常 offer、SQLite 执行日志、Broker reserve / ACK / confirm 与 mTLS 分块移交均走实际代码。Worker 可在分块之间交还控制，只有完整的原始输入摘要及预检计量通过才取得可用数据。验收在提供方用实际收到的音频运行固定的 Whisper small，核对文本摘要与完整时间段。测试专用暂停点位于一次真实范围读取之后，便于检查已读字节是否会在撤权后泄露。Chrome 验收在真实创建后丢弃第一次 HTTP 响应，通过保存的编号查回授权并撤回。可计价 Job 创建、最终转写产物及结算仍需单独接线与验收。[用户接口](../contracts/openapi/artifacts.v1.json) · [设备协议](../contracts/proto/worker.proto)

输入能力由 Hello / Welcome 中的 transcription_input_version 单独协商。版本 1 要求现有的确认启动与不可变输入能力。匹配的 Assignment 携带 grant_id、observation_id 和输入协议版本，三者受 Assignment 摘要保护。Worker 直接从 SQLite 执行记录取得这份引用。文本与音频输入不能混在同一份 Assignment。转写 Job 使用 input_required，旧版文本绑定不能插入。调度、重新 offer、ACK、续租及实际读取都需重新验证已授权的输入。

迁移 0053 将原有会话的音频输入版本初始化为 0。替换会话或切换 epoch 会清除旧版本，只有新握手明确协商才重新开启。测试在真实的 schema 51 数据库中保持旧文本输入连接，重复迁移到新结构，验证原连接继续轮询、版本 1 节点重新连接为旧版本后失去音频能力。完整控制面回归另覆盖原有 91 个 mTLS / PostgreSQL 用例。
