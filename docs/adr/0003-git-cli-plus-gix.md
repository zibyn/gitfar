# git 访问采用混合方案：网络与历史类走 git CLI，本地读走 gix，不用 git2

服务端对远程仓库的网络操作（clone、fetch、repack、gc）以及 `log`、`show`、`blame` 三个原语都以子进程调用 git CLI；`read`、`list`、`grep` 在进程内用 gix 读对象库。原因是两边都有对方补不上的短板：gix 在 fetch 侧不完整（partial clone 的 filter 尚未合并、promisor 未实现），gix-blame 仍处 very early 阶段、性能不及 git，gix-diff 也还不能生成与 git 一致的 patch 文本；而 git CLI 每次调用都要 fork 进程，不适合 grep 这种要按文件并行解压、匹配整棵树的热路径。gix 的 `ThreadSafeRepository` 是 `Send + Sync`，可以在 rayon 线程池里每线程 `to_thread_local()` 并发读。内核实测：gix 16 线程读完整棵 HEAD 树 0.51 s，git CLI 在有 commit-graph 时 `blame -L` 0.80 s、`log -n20 -- path` 约 0.03 s，两侧都够用。

代价：服务端同时依赖两套 git 实现，镜像需内置版本固定的 git（≥ 2.33），gix 是 0.x，需锁定版本并封装在一层内部接口后面。执行器按原语划分，以后 gix 补齐某项能力时可以逐个原语迁移，对外接口不变。

## Considered Options

- 全部用 git2（libgit2）：`Repository` 是 `!Sync`，不适合数十并发读；partial clone 支持自 2020 年起未合入。排除。
- 全部用 git CLI：grep 要逐文件启动子进程或依赖 `git grep` 的输出格式与正则方言，无法与 grep-searcher 的内存并行扫描相比（`git grep` 在内核上 1.5 s，且不可控截断与排序）。排除。
- 全部用 gix：blame 与 patch 文本目前不能保证与 git 一致，fetch 侧功能不全。等 gix 成熟后按原语迁移。
