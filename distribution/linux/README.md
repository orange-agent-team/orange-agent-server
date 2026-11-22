# Orange 平台 Linux ARM64 开发包

包内提供六个 Linux AArch64 ELF 程序、该版本的迁移、接口契约和配置示例。控制面使用独立 PostgreSQL，用户和 Admin 各使用独立的 OIDC 客户端。开发 API 在本地回环地址提供服务。云执行节点另需配置平台管理的 Linux 执行池，公共 Worker 仍须通过设备和模型准入。

## 校验与启动

先与发布回执核对压缩包 SHA-256，在新目录解包后运行 `cd orange-platform && sha256sum --quiet -c SHA256SUMS`。安装有 Python 3.11 以上版本时，也可运行 `python3 verify.py` 检查文件摘要和程序执行位。请将 OIDC 示例复制到部署私有目录，设置真实开发 issuer、各自的客户端 secret、回调 origin 和 ACR。包内不含数据库、商户、真实身份或模型权重。

在两个终端分别启动以下进程。两个进程使用同一个开发数据库并自动应用随包迁移，登录和会话分别归属各自客户端。

```sh
export ORANGE_ENV=development
export DATABASE_URL="$(cat /run/secrets/orange-development-database)"
ORANGE_USER_CONFIG_FILE=/run/secrets/orange-user.json ORANGE_USER_BIND=127.0.0.1:43110 bin/orange-user-api
```

```sh
export ORANGE_ENV=development
export DATABASE_URL="$(cat /run/secrets/orange-development-database)"
ORANGE_ADMIN_CONFIG_FILE=/run/secrets/orange-admin.json ORANGE_ADMIN_BIND=127.0.0.1:43111 bin/orange-admin-api
```

Worker 控制网关另行提供真实 CA、服务端证书和私钥。算力 Worker 需要已经登记的客户端证书与节点配置。云 Host 两个程序的权限边界及执行池配置见包内 docs/development-cloud-host.md 和 docs/cloud-isolation-development.md。模型计量、对象存储、支付适配和公开代理仅在所需配置与资格验收齐备时启用。

## 维护边界

该目标面向 Linux ARM64 开发部署。发行回执记录实际验证的 Ubuntu 镜像、构建镜像、Git 提交、编译器、二进制摘要和运行用例。x86_64、真实 NVIDIA 驱动、多区域、域名、发布签名和生产商户分别验收。

## 复现构建

使用 Python 3.12 和本机私有 Docker 上下文。仓库维护的默认上下文为 colima-orange-agent，虚拟机无需挂载宿主机目录。请先在仓库根目录运行下列命令。prepare 阶段验证 Cargo 锁、ARM64 Protobuf ZIP 和 OCI 清单；原生构建容器禁用网络，在独立的可写临时层内用非 root 身份编译。

```sh
python3.12 scripts/prepare-linux-build.py --fetch
python3.12 scripts/build-linux-platform.py --pass-number 1
python3.12 scripts/build-linux-platform.py --pass-number 2
python3.12 scripts/package-linux-platform.py
```

开发发行物写入 .local/linux-platform-distributions，文件名包含摘要前缀。归档和两次原生构建分别计算 SHA-256。若将先前同一原生提交的两次构建用于后续文档或打包工具修正，须显式指定 --native-source，回执同时保留原生和打包提交。任何二进制、契约、运行配置或迁移差异都需要重新构建。

构建镜像、Ubuntu 镜像、验收用 PostgreSQL 镜像及 Protobuf 压缩包均有独立摘要。Linux 验收使用项目已有的 Keycloak 26.8.1 固定镜像，在临时容器中登记两套开发客户端。该验收命令不向宿主机发布端口。[官方 Rust 镜像](https://hub.docker.com/_/rust) · [官方 Ubuntu 镜像](https://hub.docker.com/_/ubuntu) · [Protobuf 36.2 发布](https://github.com/protocolbuffers/protobuf/releases/tag/v36.2)

licenses/inventory.json 列出 Cargo.lock 中所有登记的 registry 包、版本、声明的许可表达式和随包通知路径。清单覆盖可选及其他目标平台的依赖。部分 Rust crate 将许可证放在上游仓库根目录；这些补充文件按已发布 crate 的原始 Git 提交、Git blob 和 SHA-256 核对。r-efi 的许可及作者声明位于原包 AUTHORS 中。
