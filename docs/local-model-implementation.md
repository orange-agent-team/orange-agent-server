# 本机模型适配实施记录

范围：E04 的本机 CPU 推理切片。模型结果仍为未提交候选，尚未接入 E03 完整 Agent 循环、工具执行或账务结算。

## 固定依赖和构建

`config/local-model.lock.json` 固定已有参考库的 llama.cpp 提交、官方 Qwen GGUF 提交、491400032 字节及 SHA-256。源码来自已有 Git 对象的固定提交导出，不使用相邻参考库的未提交代码。构建与下载命令：

```sh
python3 scripts/prepare-local-model.py build --source-checkout ../projects/repos/03-inference/ggml-org__llama.cpp
python3 scripts/prepare-local-model.py download
python3 scripts/check-local-model.py
```

源码和生成物只放工作区 `.local/model-runtime`。llama.cpp 使用 MIT，官方 Qwen2.5-0.5B-Instruct-GGUF 模型卡标注 Apache-2.0；模型来自固定官方提交，三次有界重试，校验成功才原子激活，不切换镜像、不购买资源。源码导出没有 `.git`，构建阻止向上找到 Orange 仓库；上游内置 Git 版本可能显示 unknown，真实源码提交和二进制 SHA-256 由独立 `build.json` 记录。

编译关闭 Metal/CUDA/Vulkan/RPC、上游工具子进程、连接工具下载及嵌入式 Web UI。运行测试只监听127.0.0.1，使用随机 API key 文件、2048上下文、一个并行槽、两条CPU线程、禁用上下文移动和UI/MCP代理。子进程只接收最小环境，不继承账户密钥。测试始终回收自己创建的进程；不把关闭 HTTP 流等同于 GPU 回收。

## 适配边界

`orange-model-runtime` 接受限定消息、工具定义和输出上限。`LlamaClient` 只允许固定 loopback HTTP 地址、显式端口和配置模型身份；禁止代理/重定向，限制连接/响应超时及响应字节。输入计数来自实际固定模型与模板的 `/v1/chat/completions/input_tokens`，总上下文超限拒绝，不静默裁剪。

流式 SSE 按完整行处理跨分片 UTF-8/CRLF，累计响应有上限。聊天响应身份不得改变，完成后不得继续输出，未知危险增量字段拒绝。工具名称必须已声明，调用索引/ID必须唯一完整，参数必须是完整 JSON 对象；截断、缺 finish 或缺 `[DONE]` 不形成可执行候选。解析一旦失败不可通过后续终止事件恢复为成功。原生 usage 单独保存，不等同于规范账单；可用固定 tokenizer 对全文重新分词，不能逐 chunk 相加。

此时尚未支持完整工具结果历史、各工具 JSON Schema 与能力批准、所有四种模型协议、图像/嵌入/转写、生产模型注册或 Broker 租约。当前 `length` 截断响应保守拒绝；合法纯文本前缀交付和 DR-01 结构化计量仍需后续切片，不能据此开放收费。取消只证明请求停止等待；生产 CPU 沙箱/内存硬限额及真实 GPU 回收仍待 E08 实现。

参考[固定 llama.cpp 服务协议](https://github.com/ggml-org/llama.cpp/blob/b74298c109c107494b792c30af4bb8fa0ce1eb2f/tools/server/README.md)和[官方 Qwen 模型](https://huggingface.co/Qwen/Qwen2.5-0.5B-Instruct-GGUF/tree/9217f5db79a29953eb74d5343926648285ec7e67)。是否实测通过，以执行台账 MODEL-LOCAL-001 的具体证据为准。

补充：已有源码词表可独立构建`llama-tokenize`，现已用它完成[DR-01候选投影与七组固定计量样例](structured-output-metering.md)。这不依赖权重下载，不改变完整模型推理仍未验证的状态；独立词表和生产模型的匹配尚待核对。

下载路径已更新为[受控Rust缓存管理](model-cache-implementation.md)，增加断点续传、私有路径、配额及取消时文件锁保护。真实网络仍超时，不能把缓存单元测试等同于已取得模型。

## MP-01 本地 Responses 文本协议

本地 Agent CLI 配置可以设置 `"model_protocol": "responses"`。省略时继续使用 `chat_completions`。两个协议共用固定模型和私有本地传输。Responses 模式从完整的用户消息、已提交的助手函数调用与相同 call_id 的工具结果构造无状态请求。输入计量使用同协议的 `/v1/responses/input_tokens`，上下文和最大输出都在派发前检查。

模型返回的文本、函数参数和最终输出须逐项匹配。缺少完成事件、调用编号发生变化、收到未声明的输出类型、模型标识不匹配、使用量矛盾或达到固定后端的输出上限都会中止本轮。该本地配置支持文本与声明的函数工具，结构化最终答复、托管工具与服务商存储均未启用。取消会中止客户端请求，资源回收另由监督进程核对。

以下命令在本机使用固定 CPU 模型和真实 SQLite 会话，验证两轮模型生成、测试所有者批准、文件读取与进程重开后的恢复。

```sh
python3 scripts/check-real-agent.py --model-protocol responses
```

## MP-03 本地向量接口

固定的 `multilingual-e5-small-onnx-cpu-v1` 配置通过私有 Unix socket 提供 `POST /v1/embeddings`，Rust 客户端入口为 `LlamaClient::embed`。请求可传单个文本或 1 至 32 个文本，编码为 float，输出维度 384。每段上限 512 个 token，包含模型特殊 token。显式的 query 与 passage 前缀按原文保留，普通文本默认补上 query 前缀。私有计量扩展 `POST /v1/embeddings/input_tokens` 返回规范输入摘要、逐项和整批计数。计量包含前缀和边界 token，排除批内补齐。计数本身不执行向量计算。[发布者模型说明](https://huggingface.co/intfloat/multilingual-e5-small)

E04 在本地私有目录创建套接字，将临时凭据经启动者的专用管道交付。每次请求调用一个固定 E09 CPU 子进程，核对 ONNX 与 tokenizer 的完整 SHA-256，再对整批执行平均池化和 L2 归一化。Rust 核对索引、行数、固定维度、有限数值、单位范数和用量。单槽被占用时返回 429。输入含过长段落时返回 422 并拒绝整批。实际分词数汇总为 usage，开发配置未接入资金账本。

```sh
python3 scripts/prepare-local-embedding.py
(cd backends && uv sync --locked --python 3.12 --extra embedding)
python3 scripts/check-local-embedding.py
```

启动器使用 `orange-embedding-host <虚拟环境 Python 的绝对路径> <模型缓存目录的绝对路径>`，对应的 Cargo feature 为 `embedding-host`。父进程保持 stdin 打开并独占读取 stdout 中的会话凭据。关闭 stdin 后服务等待在途子进程退出并回收套接字。该版本每批重新装载权重。GPU、持久预热、远程调度和开发账本的嵌入计费仍需分别验收。
