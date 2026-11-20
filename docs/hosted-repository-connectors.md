# GitHub 与 GitLab.com 只读源

CN-01 与 CN-02 共用 E07 的源端适配层。注册表固定服务商、数值仓库 ID、命名空间路径、可读目录以及是否允许问题单和代码合并记录。源端每次读取前后核对身份。账户能看到的其他项目不进入列表。公开仓库可使用匿名访问，私有源凭证由宿主保管。

文件读取先解析 Git 提交，再验证树条目、类型、普通文件模式和 Git blob ID，最后对真实字节重新计算 Git 对象哈希及独立 SHA-256。GitLab 还核对文件 HEAD 的 commit、blob、size 与 content SHA-256。读取文本或二进制均不执行仓库内容。分页信息只决定下一页页号，请求 URL 由固定服务商和注册资源构造。单动作总时限 30 秒，元数据上限 2 MiB，文件上限 20 MiB。认证失败停止该源后续请求，限流遵守供应方返回的等待时间。

公开源验收使用 octocat/Hello-World 和 gitlab-org/cli。它分别检查列表、固定版本文件、问题单列表和详情，以及 PR/MR 列表和详情。另有隔离测试检查权限、仓库身份变化、凭证拒绝和限流。这些结果的范围是源端协议与注册策略；用户授权、私有厂商账户及模型调用链需分别验收。

```sh
cargo test --offline --locked -p orange-control-plane --lib repo_connector::tests
cargo test --offline --locked -p orange-control-plane --features repository-live-tests --test repo_sources -- --nocapture
```

[GitHub Git Blob 接口](https://docs.github.com/en/rest/git/blobs) · [GitLab 仓库文件接口](https://docs.gitlab.com/api/repository_files/)
