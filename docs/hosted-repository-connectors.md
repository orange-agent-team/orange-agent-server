# GitHub 与 GitLab.com 只读源

CN-01 与 CN-02 共用 E07 的源端适配层。注册表固定服务商、数值仓库 ID、命名空间路径、可读目录以及是否允许问题单和代码合并记录。源端每次读取前后核对身份。账户能看到的其他项目不进入列表。公开仓库可使用匿名访问，私有源凭证由宿主保管。

文件读取先解析 Git 提交，再验证树条目、类型、普通文件模式和 Git blob ID，最后对真实字节重新计算 Git 对象哈希及独立 SHA-256。GitLab 还核对文件 HEAD 的 commit、blob、size 与 content SHA-256。读取文本或二进制均不执行仓库内容。分页信息只决定下一页页号，请求 URL 由固定服务商和注册资源构造。单动作总时限 30 秒，元数据上限 2 MiB，文件上限 20 MiB。认证失败停止该源后续请求，限流遵守供应方返回的等待时间。

公开源验收使用 octocat/Hello-World 和 gitlab-org/cli。它分别检查列表、固定版本文件、问题单列表和详情，以及 PR/MR 列表和详情。另有隔离测试检查权限、仓库身份变化、凭证拒绝和限流。源端实测覆盖公开 API。用户授权另以真实浏览器、PostgreSQL 和受控厂商响应验证。私有厂商账户与模型调用链仍需单独取证。

```sh
cargo test --offline --locked -p orange-control-plane --lib repo_connector::tests
cargo test --offline --locked -p orange-control-plane --features repository-live-tests --test repo_sources -- --nocapture
```

[GitHub Git Blob 接口](https://docs.github.com/en/rest/git/blobs) · [GitLab 仓库文件接口](https://docs.gitlab.com/api/repository_files/)

## Run 授权

开发 User API 从 `ORANGE_REPOSITORY_SOURCES` 指向的所有者私有文件加载配置。每条登记包含 source_id、owner_id 和 registration。带 token 的条目还要求显式 credential_revision。仓库凭证不会交给模型或客户端。GitHub 与 GitLab 的 provider、资源 ID、命名空间及可读范围均参与来源摘要。

只有创建时显式设置 `allow_external_connectors: true` 的托管 Run 才能授权第三方仓库读取。省略该选项时，服务端在授予连接器权限和源端请求之前拒绝。更改已有创建请求的该字段会触发幂等冲突。用户随后应选择本次运行允许的数值仓库 ID 和有效期；数据源地址仅由宿主管理。未授权路径与对象在预留配额前拒绝。CN-01、CN-02、CN-05、CN-06 共享每 Run 的调用数和并发限制。

read_file 完成后生成与同一 Run 和源授权绑定的文件回执。下载过程重新读取固定提交中的对象、校验 Git blob 与内容 SHA-256，并在交付前再次检查会话、Run 和源授权。公开 API 与本机授权验收可分别运行。

```sh
python3 scripts/check-repository-grants.py --database-url-file .local/database-url --identity-config .local/identity/user-config.json
```

指定 `--web-repo ../orange-agent-web` 可在受控厂商协议环境运行 Chrome，验证两种来源的 12 个动作、文件下载完整性以及撤权后的页面重载。
