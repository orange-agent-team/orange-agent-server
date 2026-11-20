# Orange 本机语音转写开发包

适用环境为 macOS arm64 和 Python 3.12。压缩包含 Rust 可执行文件、E09 后端及锁定的第三方 wheel。先按服务端仓库的 `docs/local-transcription.md` 准备校验过的模型及录音缓存，再在解压目录运行以下命令。

```sh
python3.12 -I install.py /absolute/path/to/new-venv
python3.12 -I smoke.py /absolute/path/to/new-venv /absolute/path/to/verified-model-cache
```

安装器在新目录中离线创建环境。验收调用 `bin/orange-transcribe` 预检、确认并转写已知的 11 秒录音，核对实际时长、词序、时间段和语言。自选音频使用同一可执行文件，以 `measure` 保存回执，再用 `confirm` 传入该回执。运行槽使用仅当前用户可访问的目录。此 macOS 开发包的发布签名、公证及其他平台验收仍待完成。
