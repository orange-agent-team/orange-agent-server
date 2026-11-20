# CN-06 已登记 S3 数据源

E07 的源适配器公开 list、head、read 三种读取操作。开发注册项固定回环 IP、端口、region、bucket 和目录前缀。签名只使用登记的源凭证。OpenDAL 按 path-style 生成 S3 请求，专用 HTTP transport 在发送前再次核对 origin、方法、桶与前缀。它禁止重定向、环境代理和隐式云凭证。

list 使用 ListObjectsV2 的 start-after 键，每页 1 至 100 个文件。head 返回相对键、ETag、可选的版本 ID 与精确字节数字符串。read 需要已观察的 head，限制 20 MiB，通过条件 GET 并再次比对元数据，返回内容与 SHA-256。ETag 是供应方版本条件，内容摘要由平台独立计算。

本机验收使用 config/s3-fixture.lock.json 指定的 SeaweedFS 官方 macOS arm64 发行包。将摘要匹配的 weed 放入 .local/seaweedfs 后执行 `python3 scripts/check-s3-source.py`。测试创建临时目录、随机回环端口、独立的写入夹具身份与只读源身份。它检查分页隔离、空文件、Unicode 与百分号键、超限文件、源端拒绝写入、对象替换和旧 ETag 拒读。生产云账号、TLS 与多厂商兼容性需独立取证。
