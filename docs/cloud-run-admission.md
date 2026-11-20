# 托管 Run 预留与工作区租约

本地开发控制面为当前用户的 Run 建立不可变输入文件版本引用，绑定个人根预算、工作区、已准入 Host 和有效期。受理时返回 queued / requested。Host 领取后工作区进入 preparing，Run 仍在队列。该阶段只实现控制面和工作区容量管理。模型作业、工具分派、运行内容与最终模型输出属于后续的 RunStore 和云 Agent Host。

## 准入和运行

用户 API 继续使用现有 orange-user-api、独立用户会话和已配置的对象存储。需要同时配置已迁移的 PostgreSQL，以及服务端独立的 orange-cloud-host-gateway。后者从 ORANGE_CLOUD_HOST_TLS_DIR 读取 gateway.pem、gateway.key 和 platform-host-ca.pem。ORANGE_ENV 取 development，默认只监听 127.0.0.1:43107。数据库中缺少获准 Host 或未收到有效心跳时，容量查询返回不可用，创建接口返回 503 HOST_POOL_UNAVAILABLE。

平台运维先核验客户端证书，再在独立本地 Colima 环境实测固定 OCI profile；通过后写入准入记录。下面的参数引用操作者已有的开发凭据。签发的 Host 证书必须用于 clientAuth。准入工具拒绝已用作公共 Worker 的证书、冲突的主机身份、过期或已撤销的登记。证书、真实数据库连接串及令牌都应置于私有配置目录。

```sh
python3 scripts/admit-local-cloud-host.py \
  --database-url-file .local/database-url \
  --host-id dev-host-01 \
  --host-cert .local/cloud-host/host.pem \
  --platform-host-ca .local/cloud-host/platform-host-ca.pem
```

成功返回 attestation_digest。平台 Host 使用独立 mTLS 客户端和 PlatformHostPool.Poll 提交 Host 标识、资源配置、准入摘要和是否接单。真实用户与普通 Worker 没有 Host 注册接口。开发准入限额为一个并发工作区，凭据到期不超过 24 小时。只有已认证的心跳才会创建 30 秒活跃窗口。工作区的新租约独立于模型推理 Worker 和金融预算。工作区回收通知优先返回给原 Host，支持丢失原连接后的重新查询。[Host Protobuf](../contracts/proto/host.proto)

## 用户与状态

用户通过 GET /api/v1/agent/workspace-capacity 查看当前个人配额及池可用性。POST /api/v1/agent/runs 引用本人当前有效的纯文本输入、活跃根预算与期望时长。开发档位使用固定的 dev-gvisor-linux-arm64-v1，单次期限 60 至 3600 秒，默认每人最多两个尚未回收的工作区。录入、取消、查询和回收均不修改钱包或冻结预算。浏览器至多上传 262,144 字节的运行输入；未确认提交只保存上传标识、版本、摘要与原请求键到会话存储。

GET /api/v1/agent/invocations/{id} 可按原键找回已受理的 Run。DELETE 同一路径会在相同的个人配额事务锁下记录永久撤回凭据，晚到的同键创建返回 410。若已有尚未终结的 Run 则同时取消。个人文件保留独立生命周期。查询支持按本人所属的 Run 游标倒序分页。[用户接口](../contracts/openapi/cloud-agent.v1.json)

未领取的工作区取消或到期后可直接释放。领取和续约要求匹配原 Host 证书、Run、工作区、分配标识、世代号、有效期和当前输入权限。领取之后发生取消或租约过期会使旧令牌失效，工作区进入 draining，占用继续计入配额。原 Host 必须同时确认沙箱和 cgroup 已不存在，并提供匹配原分配的证据摘要，控制面才释放容量。相同回收凭据重复提交返回同一回执。已回收的工作区不能因随后取消逻辑 Run 而再次占用名额。

## 验收范围

```sh
python3 scripts/check-cloud-run-admission.py \
  --database-url-file .local/database-url \
  --identity-config .local/identity/user-config.json \
  --web-repo ../orange-agent-web --qualify-host
```

浏览器使用固定 Node 22.23.2，测试会话为开发夹具。联合验收使用本地独立 PostgreSQL、OpenDAL 实体文件、真实 mTLS 网关与已构建的用户 Web。丢弃已提交的 HTTP 回包后，浏览器分别验证原键恢复与撤回。加 --qualify-host 会实际运行本机隔离验收后才登记临时 Host。在协议测试中，具体分配的回收声明由受控测试 Host 提供；独立 OCI 验收实测的是固定探针的内核限额和清理。生产远程证明、常驻工作区运行时及 Agent 的模型执行尚未认证。
