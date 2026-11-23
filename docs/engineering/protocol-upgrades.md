# Worker 与云 Host 的协议生成及回退

服务端导出描述符的入口为 `python3 scripts/check-proto.py`。它在临时目录中生成 Worker 和云 Host 的公开描述符，通过 SHA-256 核对后原子替换 `.local/contracts/orange.pb`。带 grpc 特性的 Rust 构建还检查含源位置的描述符和两份生成的 Rust 绑定。[生成指纹](../../crates/protocol/generation.lock.json)

升级时，在独立分支同时审查 proto 源码、编译器版本、Cargo.lock 中的生成器依赖、描述符和绑定差异。检查旧客户端对字段号、服务名、方法签名和枚举的处理，并运行 mTLS Worker 与云 Host 兼容测试。对同一源码使用两个空的构建目录重现输出后，才提交新指纹和更新后的依赖。兼容失败时恢复到上一整组已验证提交；客户端继续使用固定的契约和 Runtime 发布物。GPU 资格由模型执行与回收用例单列，协议构建可直接使用 CPU。
