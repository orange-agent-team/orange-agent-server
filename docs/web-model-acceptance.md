# 网页模型任务真实链路验收

命令：`python3 scripts/check-web-compute.py --web-repo <独立Web仓库>`，Node必须为22.23.2。脚本先按该仓库固定契约构建，再准备专用loopback43127测试TLS入口，启动实际User API、同writer Gateway、独立计量器与实际Worker/固定GGUF模型。不会修改系统证书信任、创建付费资源或执行支付。

Cargo特性`web-compute-tests`明确启用浏览器组合用例，默认Worker/价格用例不要求相邻前端或Chrome。测试将原User OIDC开发配置克隆到私有临时目录，只调整本次测试来源；浏览器会话、CPU/价格/证书登记为明确夹具，不宣称完成新的真实登录或生产准入。

浏览器真实点击上传、报价、确认提交和读取结果，数据通过HTTPS/实际HTTP、PG、mTLS和固定模型处理。独立Counter再次核对原输入/输出单位，检查唯一结算和平衡账本、attempt关闭、实际模型/Counter退出。删除原输入后浏览器结果拒绝读取并清空，不恢复正文或伪造退款。

测试代理只访问指定loopback API，静态文件限制在显式构建dist内，不转发外部绝对URL或Admin路径。失败保留生成测试的截图、主区域文本和响应状态；不复制Cookie配置、IdP秘密或TLS私钥。进程清理仅使用当前持有且未回收的Child，禁止裸PID恢复/扫描杀进程。

这是CPU开发链路证据，不能替代Windows/GPU、多区域、企业试点、真实支付或完整Web Agent认证。当前客户端未知提交的确定性撤回仍未完成；对应状态必须保持待核对。

正式源码组合验收、web-compute-tests严格Clippy与全部关联回归通过，WEB-MODEL-REAL-001已单独记DONE。首次浏览器结果超时已只读核对数据库，后端实际succeeded/settled/closed；客户端专用MIME适配修复后重新通过，未改账本或重跑原任务。
