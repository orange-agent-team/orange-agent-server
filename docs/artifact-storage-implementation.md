# 私有输入对象存储与交付前置

当前实现真实文件上传、校验、不可变发布、当前权限读取、删除与回收。它是Worker输入交付的前置，不代表已经完成Worker租约授权、推理输出发布或计费结算；现有Quote执行计划的零输入对象限制保持不变。

## 技术选择

固定OpenDAL0.59.5，仅开启services-fs和executors-tokio，关闭默认网络后端。版本、许可证和源码修订记录在`config/artifact-storage.lock.json`，解析依赖由Cargo.lock固定。选择依据为[官方Fs接口](https://opendal.apache.org/docs/rust/opendal/services/struct.Fs.html)、[固定版本发行物](https://docs.rs/crate/opendal/0.59.5)及本地固定源码edac5a0f9d6748c46dc28eabd645066cdaaa5d96。

本地适配器要求真实POSIX私有目录，持有独占根锁；一个根目录只有一个活动服务所有者。根身份持久保存，目录替换、符号链接、硬链接及不匹配根身份拒绝。它不是共享文件系统多实例或Windows ACL认证。

不启用Fs.atomic_write_dir：已检查该模式的最终rename不能当作条件覆盖保证。每次传输使用不同的随机暂存/发布键，调用if_not_exists。OpenDAL延迟创建文件，故在写入/close完成后用不跟随链接的文件句柄封存权限，再同步根目录。失败路径也等待writer.close，不提前放开仍在使用的I/O槽。

## 数据与提交

0017保存根容量、artifact不可变版本、owner/upload ID映射、传输、引用和墓碑；0018保护传输身份与回收单向性，禁止给已删除对象增加引用；0019补齐未上传对象的到期事实和活动元数据配额。已应用迁移不改字节。

- CreateUpload固定upload ID、SHA256、十进制字符串长度和声明类型；同owner/upload ID异内容拒绝。
- 真实字节先写私有暂存键，精确验证长度与摘要，再按64KiB复制至全新发布键并再次验证源摘要。所有对象IO均在数据库事务外。
- 发布前重新检查当前会话、用户、owner、团队角色和独立内容ACL，并比较组织政策/ACL版本。撤权再重授予不能让旧传输继续发布。
- PostgreSQL在一个事务里确认仍有效的传输、更新available指针及写持久事件/outbox。复制成功但数据库提交失败只留下不可见、仍计入容量的副本。
- 重放PUT仍读取并验证请求字节以及现存对象摘要，不以“已有ID”直接返回成功；并发未完成写入返回TRANSFER_BUSY。
- 原始大小/摘要/owner/version和已选发布指针不可改写；删除不能恢复为available。保留不可变ID与墓碑以便查询原操作。

个人owner只属于对应当前用户；组织内容使用五角色与独立ACL，owner/admin不自动获得私有内容。UserIdentity先验证真实会话及写入CSRF，存储事务随后重新检查会话有效性。读取先校验完整文件，再检查当前权限和版本；HTTP每64KiB输出批次再次验证权限版本。已排入有界输出队列的、此前批准的字节不声称可以撤回。

## HTTP契约与运行

权威契约为`contracts/openapi/artifacts.v1.json`及`contracts/json-schema/artifacts.schema.json`。用户凭证为原__Host-orange-user cookie；每个写操作还需要Origin、X-CSRF-Token和与upload ID完全相同的Idempotency-Key。

| 方法与路径（公共前缀/api/v1/owners/{owner}） | 用途 |
|---|---|
| POST /uploads | 创建或查询同内容上传意图 |
| GET /uploads/{upload} | 当前权限下查询原状态 |
| PUT /uploads/{upload}/content | 上传精确字节；不接受文件路径或远程URL |
| GET /uploads/{upload}/content?version_id=... | 读取指定不可变版本；attachment、no-store、nosniff |
| DELETE /uploads/{upload} | 当前写权限下逻辑删除，保留墓碑 |

JSON元数据限制64KiB；对象最多100MiB，声明媒体更严格为100000000字节。类型是声明，不代表已完成媒体解码认证。Job多输入总量及实际媒体用途限制还须在Job绑定时验证。PUT请求5分钟、读取/输出60秒；I/O所有者不会因调用方丢弃Future提前释放槽。失去响应须查原ID，不能生成新ID盲目重做。

开发入口仍限定ORANGE_ENV=development与loopback。只有显式设置ORANGE_ARTIFACT_ROOT才挂载新接口；目录必须已存在且私有。可在现有PostgreSQL/Keycloak环境运行：

```sh
python3 scripts/local-artifacts.py
```

脚本使用工作区.local/artifacts及私有配置文件，不输出密码。根默认容量1GiB、活动元数据10000条，可用ORANGE_ARTIFACT_BYTE_LIMIT、ORANGE_ARTIFACT_RECORD_LIMIT在首次初始化配置；已有根的限制不静默改写。HTTP服务每60秒执行维护，退出等待维护与所有已接收I/O结束。此额度是本地根容量，尚不是完整每租户商业配额。

## 回收与限制

最多4个对象I/O、8个待处理请求、2个HTTP下载生产者；下载队列有界。暂存/发布双份容量在准备事务中预留，实际删除并确认后才释放，重复维护不能重复扣减。

维护取得全部I/O槽后才处理过期写者。未提交副本保留1小时；未开始上传的过期意图有独立系统到期事实，不伪造用户删除。逻辑删除立即禁止正文读取，已发布内容保留24小时且受活跃引用保护。清理失败保持purging/原配额，后续按同键重试。引用表已有保护，但本轮测试引用由显式夹具建立，Job/Worker的实际pin/unpin接线仍待完成。

未实现：Worker租约绑定读权限、输入模型投影/Quote绑定、输出delivered状态、按最后业务引用释放计算的完整输入生命周期、自动跨节点存储、云对象后端、完整对象列表/上传UI、实际Worker推理、物理设备清理和结算。不能用现有available状态绕过这些门槛。

## 验证证据

```sh
python3 scripts/check-artifacts.py
python3 scripts/check.py
```

已通过14项真实PG/OpenDAL/文件系统与HTTP测试，包含实际100MiB上传/复制/读取、空文件、严格媒体限额、同ID并发、容量拒绝、数据损坏/替换链接、复制后DB故障、请求丢弃、会话撤销、ACL撤权后重授予、引用/保留期、到期事实和重复回收。另有实际开发二进制启动、匿名访问拒绝、维护和优雅退出验证。身份会话与引用是明确夹具；HTTP测试使用实际本机身份发现，但不将手工种入会话当成新的真实登录认证。

修复过程保留测试断言：延迟创建导致封存时点错误已修正；测试故障DDL按生成UUID审计并符合SQLx安全字符串要求；HTTP测试补齐既定幂等头，接口同时拒绝请求键与upload ID不一致。最终源码关联证据见services_plan/execution，不以目录存在或这些局部测试代表完整G0—G6。

后续增量：文本Job的真实输入绑定与mTLS读取已在worker-input-delivery.md独立记录。原对象切片的限制与测试数量属于其提交时点；不要将输入交付扩大解释为模型执行或输出结算。
