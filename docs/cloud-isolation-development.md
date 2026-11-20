# 托管工作区隔离的开发验收

在 Orange Agent 专用 Colima Linux ARM64 虚拟机内，验收脚本使用只读 rootfs、私有网络命名空间和限额 tmpfs 运行一个固定 Rust 程序。项目的公共推理 Worker 不参与。控制器在工作负载阻塞等待时核对真实 OCI 状态、宿主进程所属 cgroup、CPU、内存与 PID 限额，再允许程序验证读写、容量和网络隔离。工作负载以 UID 65534 和零有效 capabilities 运行。前台控制器取得退出状态，核对对应实例、运行时记录和 cgroup 均已回收后写入证据。

## 本机复验

使用固定 Rust 1.97.1 及其 aarch64-unknown-linux-musl 标准库，项目专用 Colima 配置需要关闭宿主目录挂载。运行以下命令后，证据位于 .local/cloud-isolation/result.json。

```sh
python3 scripts/check-cloud-isolation.py --download
```

依赖已缓存时去掉 --download 即可离线复验。也可通过 --archive 提供同一固定包。工具包约 78 MB，其大小与 SHA-256 由 config/cloud-isolation.lock.json 校验。脚本通过标准输入向虚拟机传送经过验证的发行包、固定探针与配置。它在一次性专用目录使用 runsc 的 OCI 接口，现有 Docker 守护进程能够继续提供开发身份服务。

配置固定 runsc 20261116.0、systrap、独立 gofer 网络命名空间和禁用工作负载网络。创建阶段和生命周期分别使用 1x CPU 策略。开发验收配置使用 0.5 CPU、128 MiB 内存、32 个 PID 和 1 MiB 工作区。该配置用于验证边界，正式用户工作区的资源档位与租户配额需另行验证。尚需独立验收云 RunStore、运行分配、持久租户工作区、GPU 及生产 Linux 集群。[官方固定版本](https://github.com/gvisor/gvisor/releases/tag/release-20261116.0) · [官方安装说明](https://gvisor.dev/docs/user_guide/install/) · [运行平台说明](https://gvisor.dev/docs/user_guide/platforms/)
