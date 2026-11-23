# 专用 WSL2 推理环境策略

此目录是 Windows x64 本机贡献的开发策略，配置档为 `wsl2-reviewed-text-v1`。仅消费远程算力的用户无需准备这些文件。WSL 配置只适用于平台专用发行版。发行版维护方将模板与专用系统镜像一起审阅，分别核对 Windows 驱动和 WSL 提供的用户态 CUDA 库。[WSL 发行版设置](https://learn.microsoft.com/en-us/windows/wsl/wsl-config) · [NVIDIA CUDA on WSL](https://docs.nvidia.com/cuda/wsl-user-guide/)

OCI 基础配置使用只读根、非 root 用户、空 capability 集合、独立命名空间和临时文件系统。默认设备规则拒绝访问，只有必要字符设备及当场核对 major/minor 的 GPU 桥可读写。系统调用按白名单放行，创建套接字限 AF_UNIX。进程只能按固定地址从只读根打开已审核权重及会话密钥，监听私有推理套接字。模型输出与取消须经本地监督器处理。该配置不挂载 Windows 卷、容器服务套接字或宿主目录。[OCI Linux 配置](https://github.com/opencontainers/runtime-spec/blob/main/config-linux.md)

### 检查和生成配置

`prepare-inference-policy.py describe` 可在开发机打印版本和模板摘要。其余命令需在专用 Linux x86_64 WSL2 发行版内使用 Python 3.12。准备程序只读取发行版配置、专用 orange-worker 的 subordinate UID/GID 分配，以及 GPU 桥的内核设备记录。它不会安装驱动或修改发行版配置。

控制器准备 0700 的私有 bundle，内含独立的 `rootfs/`。镜像文件属于容器映射后的 root，禁止可写组、特殊文件、不受控链接、set-id 和扩展属性。审核的 Linux x86_64 引擎位于 `/opt/orange/bin/llama-server`；模型须匹配配置中的完整权重摘要。控制器为每次运行准备全新密钥文件 `/opt/orange/session/api.key`，包含 64 个小写十六进制字符及换行，属容器映射后的 65532:65532，权限为 0400。清单按目录顺序记录相对路径、类型、权限和文件摘要，对会话密钥仅记录用途标记。发布者及控制器必须分别保护根快照与可信摘要。

```sh
python3.12 prepare-inference-policy.py describe
python3.12 prepare-inference-policy.py measure --bundle "$PRIVATE_BUNDLE"
python3.12 prepare-inference-policy.py render --bundle "$PRIVATE_BUNDLE" --trusted-rootfs-sha256 "$APPROVED_ROOTFS_DIGEST"
python3.12 prepare-inference-policy.py verify --bundle "$PRIVATE_BUNDLE" --trusted-rootfs-sha256 "$APPROVED_ROOTFS_DIGEST"
```

`render` 仅写入该私有 bundle 的 config.json。相同配置可重试，已有配置不匹配时返回错误。正式启动前必须重新核对不可变发布快照以及目标实际运行状态。固定资源上限用于这个 0.5B 权重开发配置。目标 Windows 驱动、设备身份、实际 CUDA 推理、取消和物理回收需要逐项实测。

### 观测执行环境

`inspect-target.py` 只观测当前诊断进程。它核对挂载、有效身份、capability、seccomp 状态、设备路径与网络接口，并尝试创建一个本地 IPv4 套接字后立即关闭，全程不发起连接。缺失路径、拒绝访问和观测失败分别记录。诊断镜像中观测满足条件，也不能证明另一模型镜像已经通过 GPU、容量或隔离验收。工具的输出不签发设备租约，也不会开始接单。
