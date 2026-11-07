# 用户输入绑定与Worker租约读取

本切片将真实私有对象交付到已授权Worker，并还原完整GenerateRequest。它没有调用模型，不证明推理完成、物理清理或费用结算。当前输入版本1只接受一个application/json的GenerateRequest，最多256KiB；其他模态及多对象输入仍待扩展。

## 发起权限与不可变绑定

用户HTTP新增：

| 接口（/api/v1/owners/{owner}前缀） | 权限与行为 |
|---|---|
| POST /jobs | 当前用户会话、CSRF和幂等键；记录发起用户并要求输入绑定 |
| GET /jobs/{job} | 当前owner权限及原发起用户，仅返回Job元数据 |
| POST /jobs/{job}/input | 原发起用户、当前输入读取权限；Idempotency-Key必须等于job ID |

个人预算由对应用户使用；组织内容角色不会自动取得任意预算使用权，必须有budget_user_grants中的显式授权。预算授权的生产配置/审批界面尚未实现，测试使用明确配置夹具。旧开发owner接口不变，但不能给无发起用户的旧Job绑定客户输入。

服务端读取完整实际对象并校验当前权限，解析严格JSON（重复键/未知字段等拒绝），执行现有GenerateRequest.validate，再按E03相同的model-request-v1计算投影。invocation、model version、Quote/计划投影以及原SubmitJob摘要必须一致。

job_inputs永久绑定job、artifact/version/hash/size、发起用户、组织policy/ACL版本及投影，并在同一事务建立真实job-input:{job}引用和事件。异内容或异版本不能重绑。尚未绑定的用户Job不分配；排队取消原子释放该业务pin。运行中/unknown的释放与完整最后引用24小时生命周期仍需后续结果/清理状态机，不自动回收。

旧quote_execution_plans的空数组保留为旧运输格式兼容；新的Job输入只从经鉴权验证的job_inputs产生Assignment描述符。它不是放开任意artifact ID透传。

## 独立Worker协议

Hello/Welcome协商input_artifacts_version=1，需要执行/节点控制能力2且Gateway配置输入读取器。旧客户端不会收到带输入的Assignment。Assignment新描述符包含ID、不可变版本、SHA256、长度及类型，全部参与摘要；空输入历史命令保持原摘要。

WorkerGateway.ReadInput是独立mTLS RPC，每次最多32KiB。身份取自TLS叶证书，不接受用户Cookie或请求体actor。每次读前、文件IO后都核对：当前设备/用户/组织、Gateway归属、会话和epoch、准确Lease/fence、已提交StartGrant、未到期的Job/模型/预算、原发起用户当前权限、明确预算授权、不可变输入版本和仍有效的业务pin。

Job的有界授权可以在浏览器退出后继续；停用发起用户、撤销角色/ACL、删除对象、撤销设备、旧epoch、失效租约或关闭预算都会阻止新的读取。原用户Cookie不会发送给供应节点。返回的数据是当前合法批次，不承诺撤回已合法交付的字节，也不是硬件机密计算认证。

输入原文按整个对象验证SHA后才返回片段。RPC总并发16、读取所有者待处理8/I/O4，取消请求不会提前释放尚在进行的文件IO。读取器对已有私有根只读，可与User API写入所有者并存；它不暴露用户写接口。独立读取器自行drain，从写入者借用的读取器不关闭写入者生命周期。

E08的connect_execution_with_inputs/fetch_model_input要求本机已ConfirmStart的准确grant，受原控制期限及本机监督期限约束。所有块、版本、offset/EOF、长度、最终SHA、model version和投影均匹配后才返回GenerateRequest。部分数据或错误不会成为可调用模型的请求，也不会延长租约。

## 运行与契约

- 用户接口契约更新为artifacts.v1.json版本0.1.1；Worker协议仍为orange.worker.v1，通过可选能力扩展。
- User API按现有ORANGE_ARTIFACT_ROOT启用上传/用户Job入口。
- Worker Gateway可显式配置同一ORANGE_ARTIFACT_ROOT，以只读模式打开已初始化根；未配置时不接受输入能力。仍保持development/loopback及真实mTLS门槛。
- 新迁移0020已经应用，0001—0020不得修改字节。组织预算授权、模型审核、价格与Quote生产接口仍须继续实现，不能把测试SQL配置当成这些界面或认证。

```sh
python3 scripts/check-worker-inputs.py
python3 scripts/check.py
```

手动验证51项真实mTLS/PG控制测试（新增4项客户/供应方输入测试）、16项对象/权限/预算测试，包含用户Job HTTP路由、精确投影、启动前拒绝、旧客户端隔离、跨节点、删除/停用用户、浏览器退出、旧epoch、队列取消pin释放和组织预算拒绝。额外协议测试保护输入描述符摘要及旧摘要兼容。CA、登录会话、价格/审核目录、OS身份及设备探针是明确夹具；文件、HTTP/gRPC和数据库是真实本机运行。

## 下一步

实现始终轮询的受管Worker Host与实际CPU模型调用，再接云/本机续期、节点策略/取消、输出版本与响应提交、清理证据和结算。扩大媒体协议前须增加对应校验与用途总量限制，不能把本版文本JSON当作四模态完成。完整G0—G6及生产认证仍未完成。
