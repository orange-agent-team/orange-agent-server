# E03 内部文件安全属性包

`orange-platform-files` 是 Agent Runtime 的内部支持库，不是新服务、核心工程或仓库。服务端 Cargo workspace 由 6 个 package 增加到 7 个；共享 Runtime 源码制品由 4 个 package 增加到 5 个。11 个核心工程的产品划分不变。新增的本地 package 必须出现在 Cargo.lock、共享源码清单和消费者锁定成员中；未引入新的第三方包身份。

## 已实现边界

只接受已打开、借用期间保持有效的文件描述符，不打开环境路径，不执行命令，不授予文件权限。读取 owner/group/mode、扩展属性和 macOS ACL/flags，生成有长度分隔的 SHA-256 摘要。属性名总长 16 KiB，单值 64 KiB，总值及名称 256 KiB，最多 128 项，ACL 64 KiB；超限、权限拒绝、读取错误和可观察到的并发变化均返回错误。

`copy_security_metadata` 要求不同的单链接普通文件。调用方只能把目标设为尚未发布的自有暂存文件；失败可能已修改暂存文件的部分属性，不能将其视作可发布结果。文件内容不复制。完成后比较源与目标的全部已建模属性；不一致则拒绝。调用方写入内容后仍须重新验证，不能将一次成功视作永久保证，也不能把这个函数当作文件内容原子 CAS。

macOS 使用公开 `fcopyfile` 的 SECURITY/XATTR 标志，通过独占 RAII 状态调用，不设置 DATA。ACL 只在极小的 Apple FFI 模块内读取；缓冲区按 C long 对齐、限制容量，native allocation 在所有返回路径释放。包默认 deny unsafe，只有该模块允许逐处说明的 unsafe；其他 workspace 包保持 forbid unsafe。

Darwin 的 `acl_get_fd` 在 FILESEC_ACL 属性不存在时可返回 ENOENT。仅在该 fd API 返回 ENOENT 且同一个借用 fd 再次 fstat 成功时表示无扩展 ACL；其他 errno 原样报错，路径 API 不适用此规则。依据是 Apple 的 [acl_file.c](https://github.com/apple-oss-distributions/Libc/blob/main/posix1e/acl_file.c) 与 [filesec.c](https://github.com/apple-oss-distributions/Libc/blob/main/gen/filesec.c)，于 2026-11-08 查阅；仅使用公开 ABI 和行为，未复制实现代码。

## Runtime / 发行集成

现有文件编辑预览绑定 security_digest；重验时 owner/group/mode、ACL 或 xattr 变化都会使旧预览不可复用。此包只处理属性；E03 的独立 Apple publisher 已在真实临时文件上验证发布和保留旧 inode。恢复决策/容量释放、源版本转换及普通 Agent 工具注册仍未完成。

共享导出只从不可变 Git 提交提取此包，并以明确成员形式记录；旧提交无此包时仍导出原四成员。消费者必须锁定新归档及清单摘要，普通构建不能引用相邻仓库源码。

## 证据与限制

本机 macOS 实际临时文件覆盖：普通/扩展 ACL、mode/xattr 复制但内容不复制、同 inode 拒绝、硬链接目标拒绝、超限拒绝且目标不变、无 ACL 的已 unlink 但仍打开文件、ACL/xattr 摘要变化。Runtime 另外验证安全属性改变后旧预览拒绝。

Linux 路径有 fd ownership/xattr/mode 实现，尚未做目标编译或运行验证；尚未覆盖 Linux inode flags、文件能力等完整发布政策，因此不能据此开放 Linux 编辑发布。Windows 尚无此包后端。macOS immutable/append-only flags 等若不能精确保留则拒绝，未以该路径宣称这些 flags 可复制。继承 ACL 不一致也拒绝。各文件系统的并发一致性、配额、断电持久性和跨设备发布保证不由本包提供。
