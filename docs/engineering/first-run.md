# 首次启动与恢复

本手册覆盖 Apple Silicon 开发机和本机 Linux ARM64 虚拟机。先进入 orange-agent-server 仓库根目录。用户 Web、Admin、桌面各自的安装包和工具链独立；这里的最小闭环通过服务端与模拟 Worker 复现。

## 1. 准备并核对工具

准备 Python 3.12、Git、Xcode 命令行工具与 macOS SDK，安装服务端锁定的 Rust 1.97.1 与 Protobuf 36.2。已有的 Worker 验收也会检查 rustup 的 stable 名称是否解析到相同的编译器。准备 PostgreSQL 17 的 pg_ctl、initdb、createdb、psql，以及 OpenSSL 3。可以用 PROTOC 和 ORANGE_POSTGRES_BIN 指定已安装的位置。Rust crate 源码需匹配 Cargo.lock；新机器首次可以从官方注册表执行 `cargo fetch --locked`，离线机器应预置对应缓存。验收使用全新的 target 目录。

准备专用 colima-orange-agent Docker 上下文，使用 Linux ARM64、不挂载主机目录。Keycloak 镜像按 scripts/local-identity.py 中的 IMAGE 摘要取得并预加载到该上下文。若同时使用 Redis 和 S3，按[开发依赖清单](development-environment.md)准备其固定输入；基本 Worker 往返使用专属临时对象目录。

```sh
python3.12 scripts/developer-first-run.py map
python3.12 scripts/developer-first-run.py doctor
```

doctor 输出本机实际发现的版本及资源身份。四仓联合构建另见[工具链资格](toolchain-qualification.md)。

## 2. 从空目录走一次完整流程

```sh
python3.12 scripts/developer-first-run.py verify --environment test
```

该命令从当前 Git 提交导出源码，在独立短路径、随机端口和空编译目录冷启动 PostgreSQL 与真实 Keycloak。它构建 Worker 协议用例、签发临时证书，并验证相同输出只发布一次、用户能读取结果且没有资金结算。结束时移除匹配所有权的测试身份容器、停止该数据库，再确认资源不可用并删除专属临时目录。清理未确认时，程序保留目录并报告路径。

## 3. 留下一套可交互的开发环境

```sh
python3.12 scripts/development-readiness.py --start --wait-seconds 30
python3.12 scripts/check-development-lifecycle.py --section worker
python3.12 scripts/local-identity.py stop
python3.12 scripts/local-postgres.py stop
```

停止默认开发环境会保留本地数据库和身份配置供下次恢复。测试实例由其自己的控制目录管理。常驻环境和一次性测试均只使用回环监听。

## 环境分配

[环境映射](../../config/environment-names.json)登记环境账户范围、实际数据库角色、身份服务名称、数据库、对象前缀和域名。开发与一次性测试在不同的 PostgreSQL 集群和不同 Keycloak 服务中使用相同的结构及应用数据库名。测试的端口、控制目录、账户密钥和临时存储隔离。预发布和生产以环境及区域划分账号、身份域、数据库及对象前缀；.invalid 地址是保留名称，交付前另行提供真实授权、密钥与域名。本机验收入口拒绝对这两个环境进行操作。

## 定位失败和升级

| 现象 | 首个检查 | 恢复动作 |
|---|---|---|
| LOCAL_PREREQUISITE | 查看 component；对照 doctor 的锁定版本 | 修正该工具的路径或预置对应工件，重新运行 doctor |
| Cargo 离线缺少包 | 查看 Cargo.lock 对应的版本、校验值与 CARGO_HOME | 从可信源补齐该锁的输入，再重建空 target |
| 端口有冲突，或集群目录不匹配 | 查询 development-readiness.py 的退出状态与本地配置的端口 | 为测试选择独立实例；不要停止其他目录拥有的进程 |
| STARTUP_MIGRATION_FAILED | 只读检查 _sqlx_migrations 的版本、success 与 checksum，核对包内迁移及源码摘要 | 保留数据和日志，恢复与既有历史匹配的发布物或按备份流程恢复后重试。不要手改真实库中的校验值 |
| 401 或 MFA 不通过 | 查看独立 user/admin 配置来源和当前会话 | 核对 issuer、客户端与要求的认证级别，再执行对应登录用例 |
| Worker 无法收到输入或输出被拒绝 | 核对会话、租约及授权，以及 job 的诊断标识 | 查询原命令和撤权状态；尚未确认执行的操作保留原幂等身份 |
| 生成器或 ABI 不一致 | 运行工具链 upgrade 检查 | 将编译器、生成器、依赖锁和绑定作为同组升级或回退 |
| 测试清理失败 | 读取保留目录，确认所属 cluster 和容器标记 | 用该控制目录停止其服务，复验已不可达后再清理 |

数据库本地服务日志在 .local/postgres/server.log。进程启动错误从标准错误读取，关联排查见 [API 诊断](api-configuration-diagnostics.md)。替换开发凭据时，使用私有文件原子替换后重启其所属服务，再验证旧凭据被拒绝、新凭据只获得预期操作权限。对于 OIDC，应由其身份提供者和客户端配置共同更新凭据。一次性测试可以直接重建独立 realm。

可用 `python3.12 scripts/check-developer-troubleshooting.py` 在一次性数据库实际演练端口冲突、迁移拒绝与恢复、开发凭据替换及旧凭据拒绝。该用例只修改它自己创建的数据库。

更新时先保留已验证的提交、构建摘要和真实测试凭据，再运行[协议升级](protocol-upgrades.md)、工具链复现和本手册的首次启动。实际 GPU、商户、跨区域及生产签名仍按各自的资格检查执行。

## 环境拓扑与区域名称

在服务端仓库运行 `python3.12 scripts/developer-first-run.py topology --run-id acceptance-01` 可查看本地开发、本次测试及八组区域环境的具体名称。测试环境使用独立的一次性集群和身份服务，数据库及角色在该集群内沿用开发名称。容器名、账户作用域和对象前缀都包含本次运行标识。省略运行标识时，展示用的测试名称取 `reference`。

区域键 `us`、`jp`、`cn`、`hk` 分别对应计划中的美国、日本、中国内地及香港区域。每个区域列出预发布和生产的逻辑账户作用域、身份域、数据库、角色、对象前缀与预留域名。所有外部域名使用 `.invalid`，实际经营主体、资源地址和驻留验证单独登记。修改环境或区域后，运行 `python3.12 scripts/check-environment-topology.py` 核对展开结果和命名冲突。原有 `map` 命令继续输出未展开的模板。
