# MC-05 本机语音转写

E04 通过 `orange-transcribe` 调用 E09 的多语言 Whisper small 开发配置。Systran CTranslate2 权重为固定版本，CPU 选择 int8 计算，接收结果时核对引擎报告的实际类型。审核文件名、精确大小、SHA-256 和预处理版本见 [固定清单](../config/local-transcription.lock.json)。ModelScope 分发文件按固定提交核验，其中 model.bin 的完整摘要也经过 Hugging Face 上游核对。[模型卡](https://huggingface.co/Systran/faster-whisper-small) · [引擎](https://github.com/SYSTRAN/faster-whisper)

音频从 Rust 持有的常规文件读入。Python 子进程从私有 stdin 接收有界字节，禁用网络模型下载。解码器由文件签名选定，支持已验证的 WAV PCM、FLAC、MP3、Ogg/Opus 和 M4A/AAC 输入。只接收单音轨；拒绝格式不符、解码错误及超限输入。每个采样计入来源时长，按固定流程重采样到 16 kHz 单声道。原始音频哈希、源采样率及个数、标准 PCM 个数与哈希共同构成预检回执。毫秒字段对整段精确有理数时长仅向上取整一次，不使用压缩文件的声明时长。此配置尚待接入平台报价与账本。

confirm 要求提供预检回执。音频、计量或模型版本改变会拒绝转写。返回结果含语言、完整文本及毫秒级有序时间段。Rust 会核对文本与时间段、长度和用量后交付候选结果。每个 CPU 槽使用所有者私有目录和子进程持有的文件锁。推理中取消时，宿主终止并等待其直接拥有的子进程。验收还在实际模型载入后终止独立的启动者，验证仍然存活的子进程持续持锁，其他调用收到忙碌状态，直到该子进程真实退出。槽目录中的 last-observed.json 记录最近观察到的阶段，实际槽释放需核对内核锁和退出状态。

```sh
python3 scripts/prepare-local-transcription.py
UV_PROJECT_ENVIRONMENT="$PWD/.local/transcription-venv" uv sync --project backends --locked --extra transcription --python 3.12
python3 scripts/check-local-transcription.py
```

验收使用 whisper.cpp 的固定十一秒录音，检查已记录的参考词序、语言检测、时间段、五种编码路径、600 秒边界、100,000,000 字节边界、损坏输入、篡改模型、预检不匹配和推理中取消。语言代码目录取自固定模型的 lang_ids 与 tokenizer，本次真实识别样本为英语。GPU、其他系统、真实多语种准确率和生产收费尚需独立验证。
