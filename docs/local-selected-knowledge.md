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

每次查询最多访问预选的 32 个、每个不超过 256 KiB 的 UTF-8 文件，每次返回最多 20 条命中。索引只存在于进程内存中；再次查询时仅重建已变化文档的词项。相同内容版本在集合内共用一份全文索引，默认结果只显示一个确定的已选来源。使用路径过滤仍可单独引用另一份已授权的同内容文件。重命名只在新路径也获选时进入索引，替换文件需要重新检索。结束进程会释放索引和所有引用。重开进程需要重新提供授权文件并重新检索。该实现处理显式文件选择，跨会话的持久文档索引、目录监视及外部连接器来源另外验收。

### 协议与验证

2025-11-25 配置先进行 `initialize` 和 `notifications/initialized`。2026-07-28 配置使用每请求的版本与客户端能力元数据，并可用 `server/discover` 发现工具。stdio 使用逐行 UTF-8 JSON-RPC，支持进度通知和按请求取消。一次只接收一个正在进行的检索或读取；取消后可以继续使用同一集合。此发行物采用 stdio，远程 HTTP 授权和第三方账户需要独立认证。[旧版生命周期](https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle) · [新版协议协商](https://modelcontextprotocol.io/specification/2026-07-28/basic/versioning)

从源码运行 `python3.12 scripts/check-local-knowledge.py` 可验证中英文命中、实际文件版本、修改与删除、跨会话隔离、取消与进度。添加 `--cpu-runtime` 并指定已经准备好的本机固定模型目录，可使用真实 CPU 模型核对两次工具调用和依据实际文件生成的答案。该验证的逐次批准由测试所有者提供。读取时，测试客户端把模型可选参数限定到实际搜索返回的引用和值，校验模型提交的参数后才发给服务端。桌面原生交互、远程集合及持久 Agent 日志恢复仍按各自范围取证。

独立协议客户端位于 `scripts/knowledge-interop`。使用锁定的 Node 22.23.2 及 `npm ci --ignore-scripts` 安装仅供验收的依赖，再执行 `node scripts/knowledge-interop/check.mjs /path/to/orange-knowledge`。测试客户端的 13 个包按参考 Inspector 提交 `e3217388c8590bcf6796fd8d0528a875ade1ac1f` 的版本和完整性摘要锁定，不打入 Rust 发行文件。[参考依赖锁](https://github.com/modelcontextprotocol/inspector/blob/e3217388c8590bcf6796fd8d0528a875ade1ac1f/package-lock.json)
