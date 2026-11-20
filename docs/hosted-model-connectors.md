# 托管模型的连接器申请与批准

用户创建模型 Run 时可以选择一个当前个人数据源支持的只读动作。目录从两个已登记连接器的生产契约生成，共支持 list_views、describe_view、query_view、list_objects、head_object、read_object 六个注册名。E03 ToolCatalog 固定输入、输出、效果类型与执行环境摘要。初始模型请求只包含被选择的一个函数定义。公共推理 Worker 得到模型输入及定义；源端凭证留在控制面读取进程。

模型报价与确认沿用该 Run 的根预算。已结算且具有提交回执的模型作业才能产生可审批请求。工作区 Host 的目录派发许可独立于平台连接器许可。用户通过模型步骤查看实际提交的函数名与参数，选择自己给当前 Run 签发的有效数据源授权，然后明确批准或拒绝。PG 的视图清单和 S3 的 bucket/prefix 继续由相应源注册项决定。调用身份、参数摘要、模型作业、响应提交、Run、工作区租约和预算根在源端派发前一并核对。

决定记录先提交到 PostgreSQL，才能读取源数据。模型派发使用与公共请求键不重合的内部键空间。重复批准只核对原决定；超时且缺少可验证终态时记录 unknown。每次申请最多派发一次，发生拒绝时不读取源。终态回执使工作区回收可以独立于浏览器完成。

用户读取已提交的工具结果时，控制面重新校验原始输入、模型产物和同一个源授权。结果正文独立存储，撤权、输入删除、源凭证拒绝和到期清理可删除正文并保留操作摘要。此版本将连接器结果交给提出请求的用户查看。将源内容送入下一次模型调用仍需独立的数据出境授权、预算确认和恢复验收。

## 开发验收

在有已核验本机 CPU 模型、gVisor、专用 S3 源程序、开发 OIDC 和数据库的工作区执行以下命令。Web 仓库需使用相同固定契约快照，Node 版本为 22.23.2。

```sh
python3 scripts/check-s3-source.py --model-run \
  --database-url-file .local/database-url \
  --identity-config .local/identity/user-config.json \
  --web-repo ../orange-agent-web \
  --model-runtime-root .local/model-runtime
```

测试通过真实浏览器提交模型报价，由独立 Worker 执行 Qwen 推理，并从已提交的模型响应中取得工具申请。源端使用独立的 SeaweedFS 和只读角色。通过、拒绝、重试、工作区清理及撤权的结论来自浏览器、数据库与源端记录。两个动作族的 schema 都有验证，模型端到端样本为 S3 list_objects。
