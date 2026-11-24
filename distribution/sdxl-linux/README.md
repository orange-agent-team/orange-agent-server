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

### Debian 动态库

官方 Python Bookworm 基础镜像缺少 OpenCV 所需的 libGL。建包时用该镜像的 Debian 密钥环验证 InRelease，再核对 AMD64 Packages.xz 的 SHA-256。用镜像自带的 APT 在禁网条件下解析 libgl1 的必要依赖，锁定所缺的 28 个包。附加的 OCI 层包含签名索引对应的二进制文件、库链接、版权文本和精确的 Debian 来源清单。该层以数据文件形式组装，不执行 Debian 维护脚本。

固定发行包的 Python 安装层和最终系统层来自两个指定提交。在服务器仓库中运行以下命令可从固定的两个 Git 检出重建依赖层，再从当前干净的候选提交组装运行入口。首次构建需要 Python 3.12、uv、本地 Docker、AMD64 执行能力以及存放依赖和解包层的磁盘空间。首次下载后，保留缓存与已导入的基础镜像，移除 `--fetch` 即可完全离线重建。两个提交都必须包含在本地 Git 对象库中。

```sh
python3.12 scripts/rebuild-sdxl-release.py --cache "$PWD/.local/sdxl-release" --context default --fetch
```

该命令分别执行两次 Python 安装及两次系统层组装，核对父镜像的固定 OCI manifest，并通过 `--expected-sha256` 比对独立登记的最终 tar SHA-256，最后在不带设备的 AMD64 容器中调用已安装的推理入口。可用 `--verify-inputs` 单独检查两个源码检出和锁文件。

### 意外中断后的设备恢复

再次启动前，原有设备记录会阻止新任务占用同一槽。收到中断后，使用原来的镜像验收文件、审核登记的摘要、GPU UUID 和私有工作目录调用恢复命令。

```sh
python3.12 scripts/run-linux-sdxl-attempt.py --reconcile --context default \
  --qualification "$QUALIFICATION" --qualification-sha256 "$APPROVED_SHA256" \
  --gpu-uuid "$GPU_UUID" --work-root "$PRIVATE_WORK_ROOT"
```

恢复在设备锁内核对容器 ID、名称、镜像、尝试编号和私有目录身份。已退出的容器经两次真实驱动读数确认显存回到原基线、无计算进程后方可移除。删除请求应答丢失时可重复执行。容器仍在运行、创建请求去向不明、所有者变化或驱动不可观测时，设备继续保留占用记录。恢复路径不读模型输入、不重启任务、不提交图片或账本操作。


### NVIDIA 目标机基准

目标宿主使用 Linux AMD64、CPython 3.12、Pillow、可读驱动数据的 nvidia-smi 和能以非 root 用户使用的本地 Docker。报告目录、工作目录和只读权重目录由宿主用户持有并设为 0700。先导入已核验的 OCI 包，使用其独立登记的 qualification 摘要。在授权的单台机器上运行：

```sh
python3.12 scripts/benchmark-sdxl-device.py --qualification "$QUALIFICATION" \
  --qualification-sha256 "$APPROVED_SHA256" --gpu-uuid "$GPU_UUID" \
  --weights "$WEIGHTS" --work-root "$PRIVATE_WORK_ROOT" --reports "$PRIVATE_REPORTS"
```

基准依次执行固定输入、同种子重放、无效输入、第一次去噪进度后的实际取消，以及取消后的再生成。每次成功都独立验证 PNG 和像素摘要，记录用时及显存峰值。失败和中断保留部分报告，未测过的设备不会获得公共调度资格。恢复及基准均需本机真实驱动读数。


目标工具包包含固定版本的 Linux AMD64 Pillow wheel、许可证和带哈希的 requirements.txt。解包后可在目标宿主创建独立 Python 环境，离线安装并先运行进度、取消、产物格式和恢复测试。驱动观测由随后的真实基准单独采集。

```sh
python3.12 -m venv .venv
.venv/bin/python -m pip install --no-index --find-links wheelhouse --require-hashes -r requirements.txt
.venv/bin/python scripts/check-sdxl-target-benchmark.py
.venv/bin/python scripts/check-sdxl-device-recovery.py
```

### 产物校验

开发机可以用 Rust 组件 `orange-image --inspect-artifact < result.png` 独立读取产物。输入限 16 MiB，固定规格为 1024 × 1024 的 8 位 RGB PNG。检查包括每个块的 CRC、最后一个 IEND 的位置、完整解码、非常量像素和 SHA-256。返回字节数、媒体类型、宽高、原始字节摘要和像素摘要。解出同样像素的不同 PNG 可以有不同的文件摘要。

Linux 设备宿主在发布文件前使用独立的 Python 实现检查同一规格，并与本次完成回执比较。该检查只证明文件内容；平台还需独立核对请求人、作业、尝试、可读版本、成功终态和固定的图像报价，才能认定一次计费交付。开发环境中的本地校验不会写入平台账本。
