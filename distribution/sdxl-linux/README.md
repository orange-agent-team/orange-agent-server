# Linux AMD64 的 SDXL 离线执行环境

本目录准备单个 NVIDIA 设备使用的本地开发镜像。容器入口固定为 Python 3.12.13、Diffusers、SDXL Base、FP16、Euler、1024 × 1024 和 25 步。启动时从镜像内核对 Python 环境，从只读输入目录复制并验证已列入清单的文件。模型权重与设备驱动由宿主提供。此候选使用独立模型编号，不改变既有 Apple MPS 模型的清单和摘要。

Linux 依赖使用独立的 uv.lock 以及 CPython 3.12 / glibc 2.36 / AMD64 wheel 选择清单。每个 wheel 记录官方文件 URL、长度及 SHA-256。所选 Torch 为 2.15.0，CUDA 运行库来自该发布在 PyPI 的实际依赖。具体驱动和设备是否能完成推理需在目标机器验证。[PyTorch 官方安装说明](https://docs.pytorch.org/get-started/)

| 操作 | 产物或约束 |
| --- | --- |
| `scripts/prepare-linux-sdxl-wheels.py --verify-lock` | 重新选择目标 wheel 后与已提交选择清单比较 |
| `scripts/prepare-linux-sdxl-wheels.py --download` | 可恢复地补齐已锁定 wheel，完整校验后保存 |
| `scripts/prepare-linux-images.py --image sdxl_python --cache .local/base-oci` | 检查公开基础镜像的 manifest、config、架构和层摘要，然后导入本地容器引擎 |
| `scripts/build-linux-sdxl-image.py` | 在固定 AMD64 解释器内进行两次禁网安装，逐文件规范化并比较层摘要，保存 OCI 布局和回执 |
| 容器 `describe` / `probe` | 分别返回固定模板及实际 CUDA 观测；缺少可见目标设备时 probe 失败 |
| 容器 `generate` | 标准输入只接受限定字段，权重目录要求只读，临时目录、输出目录须按尝试隔离。候选 PNG 需要宿主核对进程退出、文件摘要和尺寸后才能交付 |

网络、系统挂载、秘密或用户目录无需传入推理进程。物理 GPU 生成、显存水位、宿主取消后的设备回收、模型许可审核及公共调度资格分别记录。

在本地 Linux 执行宿主上运行 `scripts/run-linux-sdxl-attempt.py` 时，需要提供来自独立审核登记的 qualification 文件及其 SHA-256、明确的 GPU UUID、私有权重与工作目录，以及尚不存在的目标 PNG 路径。模型输入从标准输入传递。启动器拒绝远程 Docker daemon，固定无网络、只读根文件系统、非 root 和单设备可见性，且只终止带有本次 UUID 标记的容器。

启动前记录该设备的显存与计算进程。进程退出后，只有连续两次读到无计算进程且显存使用量回到初始水平，才清除本次占用记录。读数缺失、退出不明或容器身份不匹配时保留记录供恢复核对。宿主另外使用 Pillow 独立解码候选 PNG，验证 SHA-256、尺寸和像素摘要，通过后才发布文件。当前的无设备与状态机测试不能代替真实 NVIDIA 设备回收认证。[NVIDIA 驱动观测接口](https://docs.nvidia.com/deploy/nvidia-smi/index.html)
