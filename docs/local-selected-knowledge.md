# 本地文档检索与引用

`orange-knowledge` 是 E03 的只读 MCP stdio 服务。用户在启动进程时指定所有者、集合名、工作区、必须与工作区隔离的私有目录，以及可检索文件的相对路径。客户端只能在这个选择范围内查询。目录中其他文件、符号链接、硬链接和已有工作区策略保护的路径无法成为文档来源。

例如，工作区已有 `shipping.txt` 和 `manuals/guide.txt`，且私有目录已经存在时，客户端可使用以下进程配置。生产部署可将 command 换成解包后的绝对路径。

```json
{
  "command": "/opt/orange-knowledge/bin/orange-knowledge",
  "args": ["--protocol", "2026-07-28", "local-owner", "manuals", "/home/operator/docs", "/home/operator/orange-private", "shipping.txt", "manuals/guide.txt"]
}
```

### 查询和读取

`knowledge_search` 接受 `collection_id`、`query`，可选 `path` 和 `limit`。查询规范化普通词和汉字，使用 SQLite FTS5 排序。结果含 `hit_id`、工作区内路径、完整文件的 SHA-256 和分块的绝对 UTF-8 字节边界。`truncated` 表示还有更多命中，`unavailable_sources` 表示在此次选择范围中有多少文档当前无法索引。

`knowledge_read` 接受集合名、已签发的命中 ID 和预期 SHA-256。可选的 `start_byte`、`end_byte` 是分块内相对偏移，左闭右开，必须落在字符边界。返回文本附带当前来源以及绝对字节范围。读取前会重新打开文件并检查标识和摘要。文件删除、替换或改版后，客户端应重新检索。即使同路径的新文件具有相同内容，旧引用也不能读取它。

每次查询最多访问预选的 32 个、每个不超过 256 KiB 的 UTF-8 文件，每次返回最多 20 条命中。默认索引只存在于进程内存中；再次查询时仅重建已变化文档的词项。相同内容版本在集合内共用一份全文索引，默认结果只显示一个确定的已选来源。使用路径过滤仍可单独引用另一份已授权的同内容文件。重命名只在新路径也获选时进入索引，替换文件需要重新检索。结束进程会释放索引和所有引用。默认模式下重开进程需要重新提供授权文件并重新检索。显式持久化的流程见下节。目录监视及外部连接器来源另外验收。

### 协议与验证

2025-11-25 配置先进行 `initialize` 和 `notifications/initialized`。2026-07-28 配置使用每请求的版本与客户端能力元数据，并可用 `server/discover` 发现工具。stdio 使用逐行 UTF-8 JSON-RPC，支持进度通知和按请求取消。一次只接收一个正在进行的检索或读取；取消后可以继续使用同一集合。此发行物采用 stdio，远程 HTTP 授权和第三方账户需要独立认证。[旧版生命周期](https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle) · [新版协议协商](https://modelcontextprotocol.io/specification/2026-07-28/basic/versioning)

从源码运行 `python3.12 scripts/check-local-knowledge.py` 可验证中英文命中、实际文件版本、修改与删除、跨会话隔离、取消与进度。添加 `--cpu-runtime` 并指定已经准备好的本机固定模型目录，可使用真实 CPU 模型核对两次工具调用和依据实际文件生成的答案。该验证的逐次批准由测试所有者提供。读取时，测试客户端把模型可选参数限定到实际搜索返回的引用和值，校验模型提交的参数后才发给服务端。桌面原生交互、远程集合及持久 Agent 日志恢复仍按各自范围取证。

独立协议客户端位于 `scripts/knowledge-interop`。使用锁定的 Node 22.23.2 及 `npm ci --ignore-scripts` 安装仅供验收的依赖，再执行 `node scripts/knowledge-interop/check.mjs /path/to/orange-knowledge`。测试客户端的 13 个包按参考 Inspector 提交 `e3217388c8590bcf6796fd8d0528a875ade1ac1f` 的版本和完整性摘要锁定，不打入 Rust 发行文件。[参考依赖锁](https://github.com/modelcontextprotocol/inspector/blob/e3217388c8590bcf6796fd8d0528a875ade1ac1f/package-lock.json)

## 保存集合并在重启后读取

需要跨进程恢复时，在版本参数之后、所有者之前显式加入 `--persist`。未指定该参数时仍然使用原来的临时集合。持久模式把被选中文件的版本、分块边界、已签发引用以及用每库私有密钥生成的 HMAC-SHA-256 词项写入专用 SQLite 文件。相对文件名也以带密钥的标识保存；原文和查询字符串留在内存中。密钥和撤回状态保存在独立的授权数据库中。默认两份数据库放在同一私有目录，也可用 `--authority-dir` 将授权库放在另一个受保护的位置。取得索引和密钥的人员仍能分析词项；该格式不提供全盘加密。

下一次启动时必须传入有效的工作区授权、相同所有者、集合、工作区身份和文件选择。读取旧引用前，程序重新打开当前文件，核对对象身份、创建和变更时间、内容摘要及实际 UTF-8 分块。持久搜索会刷新该集合的已选来源，只有当前有效的来源才能参与返回。同内容文件共用词项但拥有独立的来源引用。多个进程不能并发持有同一私有库。无法可靠取得持久对象身份的文件系统会拒绝持久恢复。

```sh
orange-knowledge --protocol 2026-07-28 --persist --authority-dir /home/operator/orange-authority local-owner manuals /home/operator/docs /home/operator/orange-private shipping.txt manuals/guide.txt
orange-knowledge --withdraw-saved local-owner manuals /home/operator/orange-private /home/operator/orange-authority
```

EOF 表示关闭当前进程并释放文件句柄。撤回先在授权库内持久作废密钥，再清除缓存的词项、版本和引用。恢复旧索引时必须提供当前授权库；它已撤回时，程序在拒绝读取前清理还原出的记录。缺少授权库也会拒绝已有索引，且不会为备份补发密钥。恢复流程须单独维护最新授权库；将整个目录和授权库一同回滚尚需独立的可信水位。命令行在原进程退出后撤回；持有集合的原生宿主也可以调用异步 `withdraw()`，它会阻止新交付并等待已开始的交付完成。工作区自身的授权仍由宿主独立检查。工具调用方无法通过模型参数开启持久化或撤回其他集合。

私有目录和已存在的数据库、锁和旁文件要满足本机访问控制。Unix 目标检查所有者和组/其他用户权限；Windows 路径另检查重解析点、所有者及可继承文件 ACL。Windows 目标运行时和具体文件系统仍需实测。跨设备同步、整个授权库的灾难恢复水位，以及远程 HTTP 读取另行验收。
