# 供需指标字典

PRD-006.01。一天的边界取 UTC；也可报告明确给出的其他半开窗口 [from_unix, until_unix)。as_of_unix 表示归属、退款和结算事实的核对截止时间。重跑需要同一群体、时间精度和原始快照。

| 字段 | 计数单位与定义 |
|---|---|
| registered_nodes | 窗口结束前已登记的去重节点；离线或暂停仍计入累计登记 |
| effective_nodes | 窗口中至少有一秒合格供给的去重节点。供给者同意、当前资格、连接与可调度配置须同时有效 |
| completed_nodes | 在窗口内完成交付的胜出尝试所属节点去重。与注册数及供给者账户数分别统计 |
| eligible_slot_seconds | 每个节点、槽位分别求合格区间并集，再扣除暂停、断线和隔离区间的并集。截到报告窗口后累加。合格执行中的忙碌时间也在分母内 |
| paid_slot_seconds | 产生正数结算的胜出尝试与合格区间的交集。报价、预留、失败、重复尝试和未确认结果不会产生计费工作秒 |
| paid_outside_eligible_seconds | 胜出计费区间落在合格范围外的秒数。出现该差异时，保留时长、暂缓出具利用率 |
| utilization | paid_slot_seconds / eligible_slot_seconds 的精确分子与分母；分母为零时返回 null |
| submitted / delivered / failed / cancelled | 由各自事件时间归入窗口，按逻辑 Job 唯一身份计数。取消、失败及尚在途的作业必须分别解读 |
| attempts / settled | 前者按尝试开始计数，后者按一次真实结算计数；失去竞争的尝试可以产生成本 |
| purchased_credit_consumers | 窗口内确实使用购买分桶的付款方数量。个人与团队分别计数 |
| purchasers / repeat_purchasers | 窗口内有有效购买的付款方；其中在窗口结束前已有至少两笔有效购买者为复购。历史首购可在窗口前，全额退回或已全额拒付的购买按 as-of 剔除。复购率分母为 purchasers |

缺少原始供给历史时，快照声明 supply_history_complete=false，报表返回空供给指标。只看当前在线状态不能重建过去的可用时长。settled_payer_cohort 仅表示已核对的付款方与结算样本；完整需求量、失败率和节点利用率须用完整活动快照计算。相同快照不能把已交付次数当作全平台需求量。
