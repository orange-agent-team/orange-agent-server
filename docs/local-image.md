# MC-04 固定 SDXL 本机图像配置

此开发配置运行在 macOS arm64 的 MPS 设备上。E04 的 Rust 命令读取提示词与种子，E09 的专用 Python 子进程负责推理。单次请求固定输出一张 1024 × 1024 RGB PNG，使用 SDXL Base 1.0、EulerDiscreteScheduler、25 步、引导系数 5、fp16 权重、VAE 分块与模型定义的上转精度、隐形水印。两套 CLIP 分词器分别计入特殊 token，超过任一套的 77 token 窗口均拒绝。

模型固定为 Stability AI 的提交 462165984030d82259a11f4367a4eed129e94a7b。准备程序从 ModelScope 同名仓库的固定提交 430c0c115c292c1ff9d91bb78f9b66a91fc7f77a 获取 19 个文件，共 6,941,201,645 字节。大文件按上游 LFS SHA-256 对照分发清单，普通文件另验上游 Git blob ID。许可文件纳入同一清单。运行时仅装载已校验的私有副本，模型分发与开发二进制分开管理。[上游模型与许可](https://huggingface.co/stabilityai/stable-diffusion-xl-base-1.0/tree/462165984030d82259a11f4367a4eed129e94a7b) · [固定配置](../config/local-image.lock.json)

从仓库根目录准备模型和运行环境。macOS 图像依赖使用单独的 uv 锁；锁内仅环境描述，不新增核心工程或可分发的 SDK 包。需要 Python 3.12、可用的本机 MPS 和 Rust 1.97.1。

```sh
python3 scripts/prepare-local-image.py
UV_PROJECT_ENVIRONMENT="$PWD/.local/image-venv" uv sync --locked --project backends/environments/sdxl-macos --python 3.12
cargo build --locked -p orange-model-runtime --features image-host --bin orange-image
install -d -m 700 .local/image-slot
echo '{"prompt":"A ripe orange on a white ceramic plate.","seed":7341}' |
  target/debug/orange-image "$PWD/.local/image-venv/bin/python" "$PWD/.local/image-model" "$PWD/.local/image-slot" .local/orange.png
```

每次调用需指定新的目标文件。当前用户独占运行槽。宿主只接受本次 attempt 的校验、加载、25 个连续步进及最终回执，检查编码摘要、严格 PNG 分块、尺寸、色彩格式和独立解码后的完整像素摘要。候选文件通过校验后才在目标目录原子发布。SIGINT 或 SIGTERM 到达宿主后会终止其拥有的子进程并等待回收。公开进度不包含提示词和图像内容。

`python3 scripts/check-local-image.py` 覆盖非法选项、种子、超长输入、哈希错误、槽争用、既有输出保护、两次固定输入的真实生成，以及首次实际扩散步后取消。报告中的内存数据为同步回调处的 MPS 观察值。硬件、系统或框架改变后的像素一致性需分别测量。Windows/NVIDIA、Linux OCI、远端 Worker、付费交付和容量仍需各自的真实证据。
