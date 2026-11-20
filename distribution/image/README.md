# Orange 本机 SDXL 开发包

运行环境为 macOS arm64、Python 3.12 和可用的 Apple MPS 设备。安装器创建全新虚拟环境，校验并离线安装随包 wheels。模型权重单独准备，需符合 `config/local-image.lock.json` 的固定清单和上游许可。

```sh
python3.12 -I install.py /absolute/path/to/new-image-venv
python3.12 -I smoke.py /absolute/path/to/new-image-venv /absolute/path/to/approved-sdxl-cache
```

验收以固定种子运行 25 步，核对完整 1024 × 1024 PNG 及像素摘要。自行生成时，为 `bin/orange-image` 依次提供虚拟环境 Python、模型缓存、0700 私有运行目录及全新的目标文件名，通过 stdin 提供 prompt 和 seed 两个 JSON 字段。命令成功后输出本次生成回执。
