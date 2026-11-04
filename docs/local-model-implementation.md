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
