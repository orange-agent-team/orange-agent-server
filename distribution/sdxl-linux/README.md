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
