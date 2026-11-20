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

`python3 scripts/check-transcription-input-grants.py` 使用真实 PostgreSQL、私有对象存储、Whisper 录音、OIDC 发现、用户 HTTP 路由与 mTLS 网关。用户会话、已审核的开发 profile、排队 Job 及 Worker ACK 分别是测试夹具。正常 offer/start、分块 I/O、实际摘要检查和权限判定均由服务代码执行。测试专用暂停点位于一次真实范围读取之后，便于检查已读字节是否会在撤权后泄露。该配置尚待接入可计价的远程 Job 创建、转写交付和结算。[用户接口](../contracts/openapi/artifacts.v1.json) · [设备协议](../contracts/proto/worker.proto)
