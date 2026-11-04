# 真实本机模型与Agent闭环

2026-11-05。以下为固定小模型的实际CPU集成验证，不代表生产模型上架、完整Agent范围、商业收费或GPU认证。

## 固定制品与官方分发

模型仍是原锁定Qwen2.5-0.5B-Instruct-Q4_K_M：491400032字节，SHA-256为`74a4da8c9fdbcd15bd1f6d01d621410d31c6fc00986f5eb687824e7b93d7a9db`。原Hugging Face revision保持`9217f5db79a29953eb74d5343926648285ec7e67`。经过官方发布元数据核验，增加同一制品的ModelScope固定revision `2e50b77b0eee3083842019e257b74854323d880a`，没有更换模型内容。[Qwen官方分发说明](https://qwen.readthedocs.io/en/v2.5/getting_started/quickstart.html)、[Qwen官方ModelScope仓库](https://modelscope.cn/models/Qwen/Qwen2.5-0.5B-Instruct-GGUF)

通过既有Rust缓存下载：显式选择来源、精确HTTPS主机名单、有界重试、大小/GGUF标识/完整SHA校验和原子启用。官方API实际跳转到`cdn-lfs-cn-1.modelscope.cn`，只增加这个精确主机。此前出现重定向拒绝和一次403；使用真实Orange客户端标识后完成下载，但没有足够服务端证据将403断言为某个确定根因。未提交账号、浏览器Cookie或付费模型请求。

```sh
python3 scripts/prepare-local-model.py download --download-source modelscope
python3 scripts/check-local-model.py
python3 scripts/check-real-agent.py
```

原下载来源仍可显式选择huggingface。来源构造测试拒绝其他发布者、不同SHA/大小、浮动revision和未知来源；默认不会自动切换镜像。元数据和选定文件信息记录于工作区执行证据，配置见`config/local-model.lock.json`。

## 实际模型验收

llama.cpp仍使用固定`b74298c109c107494b792c30af4bb8fa0ce1eb2f` CPU-only构建。受监督子进程通过私有Unix socket完成真实推理；输入计量与原生usage一致。观察样例为input39、原生output2、可见文本重分词1，二者不是同一个计费口径。没有据此收费。

上下文超限和预取消拒绝、模型退出及stopped记录均已实际通过。最初验收在推理成功后因测试Sender已没有Receiver而失败，修正为先保存取消值再建立新接收者；保留了原拒绝断言，没有吞掉错误。具体通过证据是MODEL-LOCAL-001日志。

## 实际Agent闭环

`check-real-agent.py`创建自己的临时工作区和随机校验文本，模型只收到文件名与任务，不预先得到文件内容。它启动真实CPU模型，通过开发CLI执行：

1. 模型提出读取工具请求，完整响应与意图先持久化。
2. 未批准的step明确失败；产品CLI没有新增自动批准。
3. 测试驱动仅批准自己目录中的受限读取，真实文件内容成为工具结果。
4. 模型根据工具结果输出校验文本；重开CLI/日志后show和step不新增调用或结果。
5. 关闭自己的监督程序，确认实际模型stopped。

通过样例为`ready → tools_pending → ready → completed`，2次模型调用、1次明确批准、1个已提交工具结果，随机文本匹配，重开不重放。不能把这个单场景解释为通用工具质量评测。

首次真实提议的4个已知参数被原有范围校验拒绝；没有产生读取效果。随后补清模型可见的整文件默认行为、字节/行选择互斥、end_line依赖与已知哈希要求，运行时原拒绝规则保持不变。开发CLI只记录拒绝参数的形状和已知字段名，不打印模型文本、参数值或未知字段名。

后续仍包括完整工具/连接器、云Host、桌面与Web聊天交互、付费计量提交、生产身份、模型质量评测、OS托管模型服务和实际GPU清理。当前CLI场景的身份与批准是明确的本机测试所有者，不是生产SSO认证。
