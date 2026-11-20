# Worker 转写运行时资格探测

节点运营者可运行本机一次性探测，以确定 E08 所见的 CPU 转写配置。该命令读取已核验的公开录音，不读取用户对象，不访问平台网关。需要准备 E09 转写虚拟环境、权重缓存和私有运行槽。[运行环境](local-transcription.md)

在 0700 的配置目录内建立权限 0600 的 JSON 文件，提供三个绝对路径。

```json
{
  "python": "/absolute/path/to/transcription-venv/bin/python",
  "model_cache": "/absolute/path/to/transcription-model",
  "private_slot": "/absolute/path/to/asr-slot"
}
```

```sh
cargo build --offline --locked -p orange-worker --bin orange-worker
target/debug/orange-worker probe-transcription /absolute/private/probe.json
```

Worker 在实际转写前后分别观测 Python 运行时，核对 28 个包的版本、解释器文件摘要及六个关键模块的字节摘要。它通过正常 MC-05 宿主预检并转写固定 11 秒录音，检查 176,000 个采样点、完整词序、语言和时间段，再导出配置描述与 execution_profile_digest。描述绑定模型文件与处理配置、受管 runner、实际 CPU 精度、线程、容量限制和运行环境。路径、提示词及文本内容不进入回执。

可在配置里附加 expected_execution_profile_digest，只接受匹配这个固定值的配置。真实测试另在不同路径以离线、非 editable 安装重建环境，验证两份观测相同，以及解释器组件、录音、许可摘要或配置权限变化时的拒绝行为。已登记的资格目标为 macOS arm64 CPU。此记录是本机观测，后续还需在平台绑定独立审核的模型发行、节点身份、当期资源与执行租约，才能接收远端转写 Job。
