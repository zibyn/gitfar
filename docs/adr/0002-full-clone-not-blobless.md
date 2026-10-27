# 缓存一律完整克隆，不用 blobless partial clone

服务端为每个远程仓库保存一份**完整**的 bare 克隆（不带 `--filter`），fetch 后固定写 commit-graph。早期调研倾向 blobless（`blob:none`）+ 按需补 blob 以节省磁盘和首次克隆时间，但 Linux 内核实测推翻了这个前提：blobless 上的 `blame` 逐对象懒加载回源，110 s 后被 GitHub 429 限流失败；grep 也必须先补齐整树（约 47 s），节省不了什么。完整克隆内核仅 6.5 GB（blobless 2.1 GB），体量可接受。代价是首次克隆更慢、磁盘更大，由预热与"冷启动返回可重试 `repo_warming`"兜底。

## Considered Options

- blobless + 批量补取（收集缺失 OID → `git fetch --stdin`）：blame/历史类原语不可用，且需额外维护补取层，排除。
- 按前缀配置克隆模式：增加配置与两套代码路径，v1 不做。
