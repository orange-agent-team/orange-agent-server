# 本地依赖就绪检查

在拥有开发配置的服务端仓执行 `python3 scripts/development-readiness.py --start --wait-seconds 30`。启动器在仓库的 .local 目录协调并发启动，依次使用已有的 PostgreSQL 与 Keycloak 管理脚本。已有健康实例可以直接使用。仅查询时运行 `python3 scripts/development-readiness.py`，也可用 --environment 指向已有开发配置。

| 资源 | 默认端口或分配方式 | 账户与权限 |
|---|---|---|
| PostgreSQL | 127.0.0.1:55432，数据库 orange_dev | 本地账户 orange_local。探针以只读事务核对库名、用户、服务实际地址、端口和数据目录。业务测试可创建独立短生命周期数据库。 |
| Keycloak 开发身份服务 | 127.0.0.1:44180，orange-dev realm | 用户和运营各有独立 client 和回调来源。探针检查公开发现文档和签名公钥。登录、令牌交换与 MFA 另做端到端验收。 |
| 用户与运营前端 | HTTPS 回环 43122 和 43112 | 私有配置分别位于 identity/user-config.json 和 identity/admin-config.json，应用内部角色由服务端判定。 |
| S3 兼容测试存储 | check-s3-source.py 使用已审核的 SeaweedFS 和临时端口 | 独立 fixture、临时凭据、专属 bucket，测试结束回收。此基础探针只检查常驻数据库和身份服务。 |
| Redis | 容器内 6379，宿主随机分配 127.0.0.1 端口。仅用于可丢失缓存。 | 每个实例生成独立键命名空间和 256 位随机口令。默认账户关闭。客户端只能读写该实例的键前缀。可靠投递继续使用 PostgreSQL outbox/inbox。 |

退出状态 0 表示已就绪，1 表示依赖不可用，2 表示配置不合要求。输出仅含公开状态。--wait-seconds 在临时故障时重试，配置错误立即返回。启动失败可单独运行 `scripts/local-postgres.py start` 或 `scripts/local-user-identity.py` 查看对应服务的诊断。健康检查不需要真实商户或 GPU。

## 可选缓存、对象存储与 Worker

首次运行 `python3.12 scripts/prepare-linux-images.py --image development_cache` 取得内容寻址的 Redis 镜像。使用 `python3.12 scripts/local_cache.py start --instance demo` 与 `status --instance demo` 获取回环端口及私有连接文件路径；文件内的 password 仅供本地开发客户端读取。用 `stop --instance demo` 结束该实例。重复启动复用健康进程，重建后发放新的键前缀和口令。缓存使用 tmpfs，容器生存时间上限为 30 分钟。磁盘上的旧连接文件可由 stop 回收。镜像来源为 Docker Official Images 的 library/redis，Linux ARM64 manifest 与配置摘要见[版本锁](../../config/development-cache.lock.json)。

S3 服务使用本地固定版本 SeaweedFS。运行 `python3.12 scripts/prepare-s3-source.py` 准备测试二进制。随后运行 `python3.12 scripts/check-development-lifecycle.py --section services`，会启动真实 S3 与 Redis、分配临时端口并检查认证和数据操作。 Worker 用例由 `--section worker` 启动，临时 CA 签发测试证书，mTLS 客户端模拟推理回执。可重复输出提交只能产生一次可读结果；该协议测试不计费。运行 `--section restart` 验证一次性数据库与缓存被回收、替换实例为空、同机相邻命名空间的数据仍然存在。

冷启动验证命令为 `python3.12 scripts/check-development-cold-start.py`。它从固定提交导出脚本，选择独立端口和专属身份容器，验证用户与 Admin 在真实 realm 中分别注册、重启后客户端 ID 与数据保持一致，并检查另一个目录无法停止该实例。
