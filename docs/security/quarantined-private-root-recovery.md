# 私有对象根恢复

恢复程序将经过验证的 PostgreSQL 自定义格式备份装入新建的本机数据库，在开放任何业务服务之前，重放由独立恢复密钥签名的删除清单。它覆盖一个私有对象根。完成后的数据库拒绝所有新连接，后续开放流量仍需核对其他对象根、用户撤权、Job、租约、预算和账本。

在权威来源生成清单时，读取者需要能读取对象元数据。RSA 签名私钥来自独立的恢复密钥保管方。生成的 `deletions.jsonl` 包含已删除对象的标识、版本、所有者、内容摘要和删除时间。该目录应作为敏感元数据保存在独立于原数据库及原对象备份的位置。将命令输出的清单摘要写入受信任的恢复检查点。

```sh
python3.12 recover-private-artifacts.py export \
  --database-url-file private/source-url --root-id "$OBJECT_ROOT_ID" \
  --signing-key private/recovery-signing.pem --destination private/deletions
```

准备 PostgreSQL 的 `pg_dump --format=custom` 备份和对应的私有对象根备份。保留原对象 ACL。分别从受信任的备份清单和最新恢复检查点取得两个 SHA-256。恢复端使用受信任的公钥验证 RSA-PSS/SHA-256 签名和完整清单，再验证备份文件。即使某个旧清单签名有效，当其摘要与指定检查点不匹配时也会拒绝。签名不能证明签发后没有新的撤销；切换写入区域时需要独立的水位及旧写者隔离证明。

```sh
python3.12 recover-private-artifacts.py restore \
  --database-url-file private/maintenance-url --root-id "$OBJECT_ROOT_ID" \
  --dump private/backup.dump --dump-sha256 "$TRUSTED_DUMP_SHA256" \
  --journal private/deletions --manifest-sha256 "$TRUSTED_MANIFEST_SHA256" \
  --verification-key private/recovery-public.pem --destination private/restore-attempt
```

目标目录仅用于本次恢复。程序生成独立数据库名，撤销 PUBLIC 的连接权，以单事务恢复原备份，再使用包内 User API 的离线迁移入口。恢复期间不启动 HTTP、Worker 或计费进程。在一个事务中核对对象根、版本、owner 和摘要，抑制历史副本、撤销对应团队资源，并记录独立恢复回执。备份后产生而又删除的对象也保留禁止重建的记录。未确认物理清理前保留字节配额。重放不生成付款、任务及内容投递消息。

相同目录、备份和检查点可重试。已完成的恢复返回原数据库和回执。中断后先核对数据库所有者、身份标记与 OID，等待先前事务结束，再继续原数据库。事务成功而本机回执丢失时从库内恢复原统计。身份无法核对的数据库不予接管。最终回执包含库内命中数、已经删除数与旧备份中缺失的版本数。HTTP 可读性、备份轮换和多区域生产切换应在各自环境验证。
