# 上传音频的服务端预检

E07 从当前用户已获准读取的不可变对象读取实际字节。开发服务需要私有对象写入根目录和受管的 MC-05 CPU 环境。在启动 `orange-user-api` 前，将 `ORANGE_AUDIO_METER_CONFIG` 指向权限 0600、父目录权限 0700 的本机 JSON 文件。三个配置值均为绝对路径。

```json
{
  "python": "/absolute/path/to/.local/transcription-venv/bin/python",
  "model_cache": "/absolute/path/to/.local/transcription-model",
  "private_slot": "/absolute/path/to/private-decoder-slot"
}
```

槽目录需由运行服务的用户持有，并设置为 0700。权重、预处理规则和解码器依赖的准备步骤见 [本机转写说明](local-transcription.md)。每个服务进程最多接收一个同时在途的音频预检，同一槽跨进程互斥。当前会话、组织角色、对象 ACL、源版本和对象摘要在处理期间按阶段校验。只有完整解码并通过最后一次权限检查的结果才交付调用者。

用户使用已有的对象上传 API 发布音频，然后调用 `POST /api/v1/owners/{owner}/uploads/{upload}/transcription-measurements`。提供当前用户会话、Origin、X-CSRF-Token 和等于 upload 的 Idempotency-Key。请求 JSON 仅接受 version_id。响应给出原始字节摘要、解码格式和编码、精确源采样率及数量、标准 PCM 数量与摘要、整段一次向上取整的毫秒数和模型版本。声明的音频 MIME 和检测到的容器需要匹配。重复调用会重新校验授权与字节。预检本身没有财务副作用，后续平台报价须在服务端重新绑定输入版本、计量与当前授权。[接口定义](../contracts/openapi/artifacts.v1.json)

`python3 scripts/check-audio-artifact-preflight.py` 从临时数据库启动真实 HTTP 服务，验证 OIDC 发现、显式测试会话、对象上传、真实 WAV 解码、格式伪报、越权、CSRF、并发拒绝和处理中的撤权。测试启动器在已进入解码进程、执行真正的 Python 解释器前提供可控暂停点。撤权后恢复解码，响应拒绝交付计量。关闭服务后回收专用数据库。
