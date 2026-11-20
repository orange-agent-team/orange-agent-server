# MC-05 本机语音转写

E04 通过 `orange-transcribe` 调用 E09 的多语言 Whisper small 开发配置。Systran CTranslate2 权重为固定版本，CPU 选择 int8 计算，接收结果时核对引擎报告的实际类型。审核文件名、精确大小、SHA-256 和预处理版本见 [固定清单](../config/local-transcription.lock.json)。ModelScope 分发文件按固定提交核验，其中 model.bin 的完整摘要也经过 Hugging Face 上游核对。[模型卡](https://huggingface.co/Systran/faster-whisper-small) · [引擎](https://github.com/SYSTRAN/faster-whisper)

音频从 Rust 持有的常规文件读入。Python 子进程从私有 stdin 接收有界字节，禁用网络模型下载。解码器由文件签名选定，支持已验证的 WAV PCM、FLAC、MP3、Ogg/Opus 和 M4A/AAC 输入。只接收单音轨；拒绝格式不符、解码错误及超限输入。每个采样计入来源时长，按固定流程重采样到 16 kHz 单声道。原始音频哈希、源采样率及个数、标准 PCM 个数与哈希共同构成预检回执。毫秒字段对整段精确有理数时长仅向上取整一次，不使用压缩文件的声明时长。此配置尚待接入平台报价与账本。

confirm 要求提供预检回执。音频、计量或模型版本改变会拒绝转写。返回结果含语言、完整文本及毫秒级有序时间段。Rust 会核对文本与时间段、长度和用量后交付候选结果。每个 CPU 槽使用所有者私有目录和子进程持有的文件锁。推理中取消时，宿主终止并等待其直接拥有的子进程。验收还在实际模型载入后终止独立的启动者，验证仍然存活的子进程持续持锁，其他调用收到忙碌状态，直到该子进程真实退出。槽目录中的 last-observed.json 记录最近观察到的阶段，实际槽释放需核对内核锁和退出状态。

```sh
python3 scripts/prepare-local-transcription.py
UV_PROJECT_ENVIRONMENT="$PWD/.local/transcription-venv" uv sync --project backends --locked --extra transcription --python 3.12
python3 scripts/check-local-transcription.py
```

从仓库根目录运行以下命令，可对同一文件先预检，再使用已接受的回执转写。每次调用都校验输入摘要，省略最后的语言代码时由模型检测语言。

```sh
cargo build --locked --offline -p orange-model-runtime --bin orange-transcribe
install -d -m 700 .local/asr-slot
asr=.local/transcription-model
venv="$PWD/.local/transcription-venv/bin/python"
target/debug/orange-transcribe measure "$venv" "$PWD/$asr" "$PWD/.local/asr-slot" "$asr/jfk.wav" > .local/asr-receipt.json
target/debug/orange-transcribe confirm "$venv" "$PWD/$asr" "$PWD/.local/asr-slot" "$asr/jfk.wav" .local/asr-receipt.json en
```

验收使用 whisper.cpp 的固定十一秒录音，检查已记录的参考词序、语言检测、时间段、五种编码路径、600 秒边界、100,000,000 字节边界、损坏输入、篡改模型、预检不匹配和推理中取消。语言代码目录取自固定模型的 lang_ids 与 tokenizer，该十一秒回归使用英语。下面的语料验收另含普通话。GPU、其他系统、常规语料识别准确率和生产收费尚需独立验证。

macOS arm64 开发包使用相同的锁文件和后端源码。在已配置 Python 3.12 的构建机上，以下命令准备转写专用 wheelhouse，再从当前提交做两次原生构建、两次归档和一次全新离线安装。下载目录中的文件只在摘要与 uv.lock 匹配时进入发行包。

```sh
mkdir -p .local/transcription-wheelhouse
uv export --project backends --locked --no-dev --extra transcription --no-emit-project --no-header --format requirements-txt --output-file .local/transcription-wheelhouse/dependencies.txt
python3.12 -m pip --isolated download --index-url https://pypi.org/simple --only-binary=:all: --no-deps --require-hashes --dest .local/transcription-wheelhouse -r .local/transcription-wheelhouse/dependencies.txt
python3 scripts/check-embedding-distribution.py --profile transcription
```

另可运行 `python3 scripts/check-transcription-corpus.py` 检查 60 秒中英文输入。每种语言由一段已固定摘要的真人录音完整重复、句间留白，再补零至恰好 60 秒。每种输入实跑两遍，记录时长、样本数、时间段、英文词错误或中文字符错误，并采样宿主与本次子进程的 RSS。完整静音应返回 NoSpeech 且不交付候选文本。两种短录音的重复结果只用于这些固定样本的回归。[中文录音来源与参考文本](https://github.com/modelscope/FunASR/wiki/%E5%BF%AB%E9%80%9F%E6%8E%A8%E7%90%86)
