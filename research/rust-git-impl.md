# Rust git 实现选型：gix vs git2 vs git CLI

工单：[#3](https://github.com/zibyn/gitfar/issues/3)（地图 [#1](https://github.com/zibyn/gitfar/issues/1)）。调研日期：2026-10-27。
调研时的版本：`gix` 0.90.0（2026-10-22）、`gix-blame` 0.20.0、`git2` 0.21.0（2026-10-06，绑定 libgit2 v1.9.x）、git CLI ≥ 2.4x。

## 结论（先看这个）

**选混合方案：网络与对象获取一律走 git CLI；本地只读检索原语走 gix；不使用 git2。**

- **fetch / clone（含 `--filter=blob:none`、增量 fetch、protocol v2、HTTPS token、SSH）→ 子进程调用 `git`。**
  三者里只有 git CLI 今天能完整支持 partial clone + 按需补取缺失 blob。gix 的 filter 支持还停留在未合并的 PR，libgit2 自 2020 年起的需求一直没合入。
- **读取（ls/read/glob/log/rev-walk/tree diff，可能还有 grep）→ gix。** `ThreadSafeRepository` 是 `Send + Sync`，每个线程用 `to_thread_local()` 拿到自己的对象缓存，能直接撑数十并发查询，不需要额外加锁或进程池。
- **blame，以及需要与 git 输出逐字节一致的 patch 文本（`diff`/`show`）→ 先用 git CLI**（`git blame --porcelain`、`git diff`/`git show`），等 gix-blame 与 gix-diff 的补丁生成成熟后再迁移。
- **blobless 缓存下的缺失 blob**：gix 读到不存在的 blob 会直接报错，不会去远程补取。应在 gix 之上加一层：先收集本次查询需要的 OID，批量交给 `git fetch`（或 `git backfill`、`git cat-file --batch`，后两者会触发 lazy fetch）补取，然后刷新 gix 的 pack 视图再读。上游维护者 Byron 也是这个思路：先收集缺失对象，一次性取回，再计算（见下文）。

注意事项列在文末。

## 对比表

| 维度 | gix (gitoxide) | git2 (libgit2) | git CLI |
|---|---|---|---|
| clone / fetch | ✅ 支持，含 shallow [1] | ✅ 支持；v1.7 起支持 shallow [5] | ✅ 参照实现 |
| partial / blobless `filter=blob:none` | ❌ 未合并：PR #2375 仍 open [3]；跟踪 issue #1046 已关闭（NOT_PLANNED，转为讨论）[2] | ❌ issue #5564 自 2020 年 open，至今无合并 [6] | ✅ |
| lazy fetch 缺失 blob（promisor） | ❌ crate-status 中 promisor 各项全部未勾选 [1] | ❌ [6] | ✅ 自动补取；也可用 `fetch --stdin` 批量补取 |
| protocol v2 | ✅ git:// 与 http(s) 都支持 V2 握手 [1] | ⚠️ changelog 里找不到 v2 的记录 [5]（视为不支持） | ✅（2.26 起默认） |
| 增量 fetch | ✅ | ✅ | ✅ |
| HTTPS token | ✅ 支持 credential helper；HTTP 走 curl 或 reqwest，**仅阻塞式** [1] | ✅ 凭证回调 | ✅ credential helper / `http.extraHeader` |
| SSH | ⚠️ 只能调外部 `ssh` 二进制，没有原生实现 [1] | ✅ libssh2；v1.8 起可选 `USE_SSH=exec` 调 OpenSSH [5] | ✅ OpenSSH（`GIT_SSH_COMMAND`） |
| 并发读对象库 | ✅ `ThreadSafeRepository: Send + Sync`（需 `parallel` feature），按线程 `to_thread_local()` [4]；对象缓存按线程，"perfect scaling with cores" [1] | ⚠️ `Repository` 是 `Send` 但 `!Sync` [7]；libgit2 规定对象默认不可跨线程共享，只有 `git_odb` 是线程安全的 [8] | ✅ 每个进程相互独立；可用长驻的 `cat-file --batch` 降低开销 |
| rev-walk / log | ✅ 支持 tips/exclude；不支持 commit-graph 加速 [1] | ✅ revwalk | ✅（commit-graph + bloom filter） |
| tree diff / rename | ✅ tree-tree、rename/copy 检测；**文本 patch 生成未勾选**，"diff-heuristics match Git perfectly" 未勾选 [1] | ✅ 完整的 patch API | ✅ 参照实现 |
| blame | ⚠️ gix-blame 属 "very early" [9]；不支持 rename 跟踪；官方原话 "performance isn't competitive with Git" [1] | ✅ 有，v1.9 增强了 hunk 信息 [5]（大仓库下速度存疑，未实测） | ✅ 最快、最完整 |
| grep | crate-status 未覆盖（需自己基于 gix 遍历 + 正则实现，或用 trigram 索引） | 无 | ✅ `git grep`（可多线程） |
| 成熟度 | 0.x，几乎每月一次破坏性版本；仅 gix-lock、gix-tempfile 达到 tier 1/2 [9] | libgit2 v1.9 为 1.x 最后一版，v2.0 会有 API/ABI 变更（SHA256）[5] | 最成熟 |
| 许可 | MIT OR Apache-2.0 [9] | libgit2：GPLv2 + linking exception [10]；git2-rs：MIT/Apache | GPLv2（只作为子进程调用，不链接，无传染） |
| 运维成本 | 纯 Rust，静态链接 | C 依赖（OpenSSL/libssh2） | 镜像中需要 git 二进制，并管理子进程生命周期、超时、输出解析 |

## 关键证据

1. **gix 的 partial clone 状态**
   - crate-status 的 `gix-odb` 部分："[ ] fetch missing objects on demand from promisor remotes"、"[ ] support multiple promisor remotes and `extensions.partialClone`"。顶层任务 "[ ] partial clone, promisor and bundle bootstrapping" 也未勾选 [1]。
   - Roku 提交的 PR #2375 "feat: filters and partial cloning: initial support"（只有 blob filter 和 bare clone，**不包含按需补取**）至 2026-07 仍 open [3]。
   - 下游的 PR #3018（gix-tix）原话："Gitoxide does not yet provide native promisor-object fetching. Delegating Git's established multi-OID `fetch --stdin` protocol…"。这正是本文推荐的混合模式 [11]。
   - Byron 在 #1046 中写道："when handling anything with remotes, the best choice actually is to rely on the Git binary… When reading anything else though, local data, `gitoxide` should be a good choice for most." 关于按需补取，他的看法是："select algorithms can start making use of this by gathering the missing objects first, and fetching them all before starting the actual computation." [2]
2. **libgit2 不支持 partial clone**：issue #5564 中，相关 PR（#5993、#5996）均未合并，贡献者 2022 年的原话是 "nothing ever got merged to libgit2"，目标也只是**读取** partial clone，不包括创建 [6]。
3. **线程模型**
   - gix：`ThreadSafeRepository` 是 `Sync + Send`；但 "When built with `default-features = false`, this type is **not** `Send`"，需要开启 `parallel` feature [4]。
   - git2：`Repository` 为 `!Sync` [7]，因此并发读要么每个线程各开一个 `Repository`，要么加锁。
4. **blame 性能**：gix-blame 自己列出了达到 Git 水平前必须完成的工作：路径感知的图遍历、从 commit-graph 读树、bloom filter [1]。

## 对 gitfar 的含义 / 注意事项

- **blobless + gix 的缺口要自己补**：需要实现"批量补取缺失 blob"这一层，处理 gix 返回的 object-not-found 错误，补取后让 gix 重新加载 pack（`Store` 会检测新 pack，具体刷新语义需实测）。另一种做法是对热门仓库直接做 full clone，绕开这个问题。在 Linux 内核规模下，full 与 blobless 的磁盘及首次拉取代价要在存储架构工单里权衡。
- **grep 不能靠 lazy fetch 逐个补取**：在 blobless 仓库上对整个树做 grep 会触发海量单对象请求（#1046 提到 git 的 blame/rebase 有同样问题）。grep 要么在预热时 `git backfill` / 补全 HEAD 的 blob，要么依赖 trigram 索引。
- **子进程管理**：所有 git CLI 调用必须设置超时、并发上限（与对上游的限速一起设计）、`GIT_TERMINAL_PROMPT=0`、隔离的 `HOME`/`GIT_CONFIG_GLOBAL`，并按 host 注入凭证（`credential.helper` 或 `-c http.extraHeader`，配合 `GIT_SSH_COMMAND` 指定 key）。注意不要让 token 出现在进程参数或日志里，优先用 credential helper 或环境变量。
- **gix 升级节奏**：0.x 每月都可能有破坏性变更，需要锁定版本，并把 gix 封装在一个内部 `ObjectReader` 模块里，避免 API 泄漏到各处。
- **SSH**：gix 和 git CLI 都依赖外部 `ssh`，所以运行镜像本来就要带 OpenSSH，这也不构成额外成本。
- **未实测**：本文没有在 Linux 内核仓库上做 benchmark（gix 与 git 的 rev-walk、tree diff、blame 时延，以及 blobless clone 的大小和耗时）。建议在 spec 定稿前做一个小型原型来测量。

## 来源

1. gitoxide `crate-status.md`（main）：https://github.com/GitoxideLabs/gitoxide/blob/main/crate-status.md
2. gitoxide issue #1046 "Minimal implementation of partial clones & promisor objects"（2026-07-22 关闭，NOT_PLANNED，转为讨论 #2791）：https://github.com/GitoxideLabs/gitoxide/issues/1046
3. gitoxide PR #2375 "feat: filters and partial cloning: initial support"（open）：https://github.com/GitoxideLabs/gitoxide/pull/2375
4. docs.rs `gix::ThreadSafeRepository`（0.90.0）：https://docs.rs/gix/latest/gix/struct.ThreadSafeRepository.html
5. libgit2 changelog：https://github.com/libgit2/libgit2/blob/main/docs/changelog.md
6. libgit2 issue #5564 "Support for partial clones"（open）：https://github.com/libgit2/libgit2/issues/5564
7. docs.rs `git2::Repository`（0.21.0）：https://docs.rs/git2/latest/git2/struct.Repository.html
8. libgit2 `docs/threading.md`：https://github.com/libgit2/libgit2/blob/main/docs/threading.md
9. gitoxide README（许可、稳定性分级）：https://github.com/GitoxideLabs/gitoxide/blob/main/README.md
10. libgit2 `COPYING`：https://github.com/libgit2/libgit2/blob/main/COPYING
11. gitoxide PR #3018 "fix(gix-tix): fetch promised blobs on demand"：https://github.com/GitoxideLabs/gitoxide/pull/3018
12. git partial clone 文档：https://git-scm.com/docs/partial-clone ；git-clone `--filter`：https://git-scm.com/docs/git-clone
