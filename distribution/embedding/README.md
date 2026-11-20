# Orange 本机嵌入开发包

本包面向 macOS arm64 和 Python 3.12，提供 E04 原生宿主和客户端、E09 wheel 及其运行依赖。模型文件使用独立的本地缓存。缓存中应包含与 `config/local-embedding.lock.json` 匹配的 `model.onnx` 和 `tokenizer.json`。宿主在推理前校验二者的完整 SHA-256。

在解压目录中执行以下命令。为虚拟环境选择尚未存在的路径。安装器校验包内文件摘要，创建虚拟环境并使用随包 wheels 离线安装。验收脚本会启动私有 Unix 套接字，验证三段文本的向量、顺序、用量与进程回收。

```sh
python3.12 -I install.py /absolute/path/to/new-embedding-venv
python3.12 -I smoke.py /absolute/path/to/new-embedding-venv /absolute/path/to/verified-model-cache
```

长期调用可由启动器运行 `bin/orange-embedding-host <venv/bin/python> <model-cache>`，保持其 stdin 打开，从专用 stdout 管道读取本次会话的套接字与令牌。开发包限本地使用，发布签名、公证和其他目标系统的验收待完成。
