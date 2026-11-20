# CN-06 已登记 S3 数据源

E07 的源适配器公开 list、head、read 三种读取操作。开发注册项固定回环 IP、端口、region、bucket 和目录前缀。签名只使用登记的源凭证。OpenDAL 按 path-style 生成 S3 请求，专用 HTTP transport 在发送前再次核对 origin、方法、桶与前缀。它禁止重定向、环境代理和隐式云凭证。

list 使用 ListObjectsV2 的 start-after 键，每页 1 至 100 个文件。head 返回相对键、ETag、可选的版本 ID 与精确字节数字符串。read 需要已观察的 head，限制 20 MiB，通过条件 GET 并再次比对元数据，返回内容与 SHA-256。ETag 是供应方版本条件，内容摘要由平台独立计算。

本机验收使用 config/s3-fixture.lock.json 指定的 SeaweedFS 官方 macOS arm64 发行包。将摘要匹配的 weed 放入 .local/seaweedfs 后执行 `python3 scripts/check-s3-source.py`。测试创建临时目录、随机回环端口、独立的写入夹具身份与只读源身份。它检查分页隔离、空文件、Unicode 与百分号键、超限文件、源端拒绝写入、对象替换和旧 ETag 拒读。生产云账号、TLS 与多厂商兼容性需独立取证。

用户 API 通过私有配置文件 `ORANGE_S3_SOURCES` 加载登记表。每项包含 source_id、owner_id、registration、access_key 与 secret_key。用户针对显示的 bucket 和 prefix 确认授权。来源版本、当前会话、Run 存续、原输入产物状态和授权有效期均参与交付检查。两类连接器共用每 Run 100 次读取预算与 4 个并发槽。

read_object 对固定对象执行条件 GET，生成与 grant、Run、大小、版本和内容 SHA-256 绑定的源端产物回执。重试相同幂等键可取回原回执。附件字节仍由源数据桶持有。授权下载端点重新核对源版本和 SHA-256，再核对当前会话及授权，最后以 attachment 和 no-store 返回二进制内容。源对象替换或删除、授权过期、撤销、Run 结束均阻断旧引用，重新授权产生独立引用范围。[接口契约](../contracts/openapi/s3-connector.v1.json)

联合验收在两个临时回环端口运行真实 S3 源和受控延迟代理。代理只在源已返回完整的 3 MiB 后延迟交付，以验证在途撤权。平台状态存储在一次性 PostgreSQL 数据库。用户会话和运行中的 Run 是测试记录。
