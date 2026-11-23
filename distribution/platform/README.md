# Orange 本机平台服务开发包

目标为 macOS arm64。构建回执记录实际构建系统，其他 macOS 版本需单独验证。包内含六个本机进程、对应数据库迁移、接口契约和示例配置。用户服务与管理服务各自验证身份。准备独立的开发 PostgreSQL 和 OIDC 服务，向每个进程提供所需私有配置；已存在的开发环境可直接复用。模型、证书、数据库内容和服务账号均由部署者另行准备。

先使用旁附构建回执的 SHA-256 校验整个归档，再解压并运行 `python3.12 -I verify.py` 复核内容。首次连接专用开发数据库时，服务运行内置 SQLx 迁移。建立私有对象根目录时设为 0700。凭证配置文件置于 0700 的目录，文件权限 0600。

| 入口 | 配置与启动 |
| --- | --- |
| bin/orange-user-api | 设置 ORANGE_ENV=development、DATABASE_URL、ORANGE_USER_CONFIG_FILE 和 ORANGE_USER_BIND。按需设置 ORANGE_ARTIFACT_ROOT、ORANGE_AUDIO_METER_CONFIG 或已审核的计量器配置 |
| bin/orange-admin-api | 设置 ORANGE_ENV=development、DATABASE_URL、ORANGE_ADMIN_CONFIG_FILE 和 ORANGE_ADMIN_BIND。管理 OIDC 客户端及管理会话独立 |
| bin/orange-worker-gateway | 设置 ORANGE_ENV=development、DATABASE_URL、ORANGE_WORKER_GATEWAY_BIND、ORANGE_WORKER_GATEWAY_CONFIG，并提供专用 mTLS 证书。独立 Gateway 使用对象只读入口；需要输出写入时，由用户服务共同承载网关并共享对象写入器 |
| bin/orange-worker | 参数为节点自己的私有开发配置文件。设备证书、已审核模型、进程程序和用户授权需分别准备。进程在前台运行 |
| bin/orange-cloud-host-gateway | 设置 ORANGE_ENV=development、DATABASE_URL、ORANGE_CLOUD_HOST_BIND 和 ORANGE_CLOUD_HOST_TLS_DIR。平台 Host 使用独立的身份 CA |
| bin/orange-cloud-host-node | 设置 ORANGE_ENV=development 与 ORANGE_CLOUD_HOST_CONFIG，使用独立的平台 Host 配置和本机 OCI/gVisor 执行池。宿主只接收平台工作区租约。启动参数和证书部署见包内云 Host 文档 |

在两个终端分别运行 `bin/orange-user-api` 与 `bin/orange-admin-api`。服务监听各自的开发回环地址。包内保留契约、固定模型和审核连接器的目录描述。实际使用某一可选后端前，需单独安装其运行环境和权重。读取用户接口需用户会话，管理操作仍遵守独立 MFA、双人批准和审计规则。

发行验收使用一次性 PostgreSQL 数据库与实际本地 OIDC 发现，启动解包后的用户和管理进程并检查独立入口、匿名拒绝、一个已授权的开发用户会话、迁移及停机。Worker 与云 Host 的运行资格还取决于对应的设备、凭证、隔离环境和单独的集成证据。

包内 config/user-oidc.example.json 和 config/admin-oidc.example.json 给出独立的开发客户端、回调 origin 与 ACR。将示例复制到本机私有配置目录，分别填入对应客户端的私有 secret。config/worker-gateway.example.json 列出 Worker 网关需要的证书路径。

用户 API 和 Admin API 可分别设置本机回环指标端口。指标字段、取消语义和启动示例见[本机 HTTP 指标](docs/local-http-metrics.md)。

用户 API 和 Admin API 支持分层启动配置及 --describe-config。配置字段、错误分类、重试语义与作业关联方式见[配置与诊断](docs/engineering/api-configuration-diagnostics.md)。

schema 67 为个人与组织任务提供状态版本，旧客户端可以继续使用原取消接口。读取、条件取消、幂等重试及升级行为见[作业状态与版本](docs/engineering/job-state-revisions.md)。

用户作业的持久事件支持 HTTP 快照和只读 WebSocket。反向代理需将 /api/v1/owners/*/jobs/*/events/ws 的升级请求传给同一个用户 API，并保留浏览器 Origin。确认、游标和关闭语义见[作业事件](docs/engineering/owned-job-events.md)。

数据库维护随包提供[关键表与事务说明](docs/database/transactions-and-retention.md)和[业务键字典](docs/database/transaction-keys.json)。升级前请核对迁移版本与目标数据库。

用 `python3 reconcile-wallets.py --database-url-file <本地数据库连接文件>` 可从复式分录重新汇总钱包的七个余额字段。购买来源的作业预留与退款预留都计入 reserved_purchased，供应者待释放收益按分录中的供应者匹配。一个 PostgreSQL 快照同时读取账本与钱包。输出按字段报告差异数量；退出码 0 表示本次读取一致，2 表示存在差异，1 表示检查未完成。历史直接拨入的夹具余额会显示为差异，应先查清来源。此命令只读，不生成调账分录。
