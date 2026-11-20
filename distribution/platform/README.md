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
