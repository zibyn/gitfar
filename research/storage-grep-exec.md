# 服务端存储与 grep 执行方式对比

工单：[#4](https://github.com/zibyn/gitfar/issues/4)　地图：[#1](https://github.com/zibyn/gitfar/issues/1)

问题：Linux 内核量级仓库、数十并发下，任意 ref 上的正则 grep 用哪种执行方式——
(1) bare 仓库按需解压 blob 扫描；(2) 为热 ref 维护 worktree checkout 再用 ripgrep 系 crate 扫描；(3) trigram 词法索引。

## 结论（v1 建议）

**v1 选 (1)：bare 仓库 + 按需读取 blob（gix）+ grep-searcher 在内存切片上匹配，按文件并行。**
(2) 不作为独立方案，而是作为 (1) 的可选缓存层：按**提交/树 SHA**（不可变）物化快照，而不是维护随分支移动的 worktree。
(3) 推迟到 v1 之后，且只能作为"候选文件过滤器"，最终结果必须由与 (1) 相同的扫描器确认，跳过的文件必须回退扫描。

理由：

- 只有 (1) 天然支持"任意 ref"且结果直接绑定解析出的 SHA；(2)(3) 公开的参考实现都只覆盖默认分支或有限分支，其余 ref 依旧回退到扫描（见下）。
- 解压是 (1) 的主要额外成本，但可并行；本地实测在 git/git 上多线程 `git grep <ref>` 比单线程快 3.5–6.7 倍（见"本地基准"）。
- 精确性：(3) 的现成实现都会跳过部分文件（大文件、非 UTF-8、trigram 过多），直接用不满足地图 #1 的"结果须与直接扫描完全一致"约束。

主要风险：并发扫描是 CPU 密集的。按 GitHub 公布的约 0.6 GB/s/核 的 grep 吞吐估算，扫描内核 HEAD（约 1.4 GB 未压缩，**该体量为估计值，未在本次实测**）每个查询约需 2–3 核·秒，数十并发需要数十核或排队。这是将来引入 (3) 的触发条件，而不是 v1 的阻塞项。

## 方案 (1)：bare 仓库，按需解压 blob 扫描

- `git grep <tree>`："Instead of searching tracked files in the working tree, search blobs in the given trees."；`grep.threads` 未设置时 "Git will use as many threads as the number of logical cores available"。[git-grep 文档](https://git-scm.com/docs/git-grep)
- gix 可以打开 bare 仓库、读取 tree/blob；带内存上限的 LRU 对象缓存（需显式启用，`Repository::object_cache_size()`）和 pack 增量基对象缓存；`Repository` 不是 `Sync`，需 `into_sync()` 得到 `ThreadSafeRepository` 后在线程间共享。gix 没有内建 grep。[gix 文档](https://docs.rs/gix/latest/gix/)
- grep-searcher 支持直接在 `&[u8]` 上搜索（`Searcher::search_slice`），并负责二进制检测（`BinaryDetection`）。[grep-searcher 文档](https://docs.rs/grep-searcher/latest/grep_searcher/)。因此 gix 解出 blob → `search_slice` 即可组成纯 Rust 流水线，无需落盘。
- 多 ref：天然支持任何能解析成提交的 ref；结果直接带该提交 SHA。相同 blob 在不同 ref 间可以按 blob OID 缓存/去重匹配结果。
- 存储：只有 pack 本身。本地实测 git/git 浅克隆 pack 13.6 MB 对应 50.3 MB 工作树（约 27%）。
- 维护：只有 `git fetch` 与周期性 `git gc`/repack。
- 已知注意点（未实测）：完整历史的 pack 中 blob 可能有深增量链，读旧提交比浅克隆更贵；gix 的增量基缓存大小要调。
- 参考实现：Sourcegraph 的 searcher 服务处理"未索引"的 repo/revision：从 gitserver 取该提交的 zip 归档后扫描（[Life of a search query](https://docs-legacy.sourcegraph.com/dev/architecture/life-of-a-search-query)，文档较旧）。

## 方案 (2)：热 ref 维护 worktree checkout + ripgrep crate

- 扫描速度：ripgrep 作者在 Linux 内核树上的公开基准（2016，旧版本）中，字面量查询 `rg 0.334s` vs `git grep 0.345s`（EC2 c3.2xlarge），`rg 0.156s` vs `git grep 0.195s`（i7-6900K）；无字面量正则 `rg 0.577s` vs `git grep 26.382s`；Unicode `\w` 查询 `rg 0.355s` vs `git grep 13.045s`。注意这里的 `git grep` 搜的是工作树，不是 tree 对象。[ripgrep 基准博文](https://burntsushi.net/ripgrep/)
- 结论：工作树扫描比按需解压快（省掉 zlib 解压与增量还原），且 Rust regex 在 `\w`、无字面量正则上远好于 git 内建正则；但后一项优势在 (1) 中用 grep-searcher 替代 `git grep` 同样能拿到，(2) 真正额外拿到的只有"省掉解压"。
- 存储：每个 checkout 是完整未压缩树（内核约 1.4 GB 量级，估计值），每多一个热 ref 多一份。
- 维护：分支前进时需要更新 checkout，更新期间与查询存在竞争，必须做原子切换（例如新目录 + rename），否则结果与声明的 SHA 不一致。按不可变提交 SHA 物化快照可以去掉这一类问题：快照只增不改，只需按 LRU 淘汰。
- 精确性陷阱：`ignore` crate/rg 默认跳过隐藏文件并遵守 `.gitignore`；对 checkout 扫描必须关闭这些过滤，否则 `.github/` 等跟踪文件会丢失，结果不等于 (1)。二进制检测策略也要与 (1) 保持一致。
- 多 ref：只对物化过的 ref 有效，其他 ref 仍需 (1)。

## 方案 (3)：trigram 词法索引

| 系统 | 索引大小 | 延迟 | 多 ref 处理 | 来源 |
|---|---|---|---|---|
| Google Code Search（Go，`google/codesearch`） | 约为源码的 20%（Linux 3.1.3：420 MB 源码 → 77 MB 索引） | `hello world` 0.01s vs 暴力 1.96s；忽略大小写 0.08s vs 1.59s | 单棵目录树 | [Russ Cox, Regular Expression Matching with a Trigram Index](https://swtch.com/~rsc/regexp/regexp4.html)，[repo](https://github.com/google/codesearch) |
| Zoekt（Go，Sourcegraph 维护） | shard 约为语料 3.5x；搜索需约 1.2x 语料的内存 | 设计目标 "sub-50ms" 于 Android/Chrome 量级（目标非实测） | 每文件 branch bitmask，相同内容跨分支只存一次；mask 为 `uint64`，即每 shard 至多 64 个分支 | [design.md](https://github.com/sourcegraph/zoekt/blob/main/doc/design.md)，[`index/indexdata.go` 的 `fileBranchMasks []uint64`](https://github.com/sourcegraph/zoekt/blob/main/index/indexdata.go) |
| Sourcegraph（Zoekt + searcher） | 内存需"能容纳默认分支全部文本" | — | 默认只索引默认分支；`search.index.branches` 可加分支（实验性）；其余 revision 走 searcher 扫描 | [Sourcegraph search 管理文档](https://sourcegraph.com/docs/admin/search) |
| GitHub Blackbird（Rust，闭源） | 115 TB 原始 → 去重后 28 TB 唯一内容 → 25 TB 索引 | 单 shard p99 约 100 ms；64 核主机上限约 640 QPS | 按 git blob OID 分片去重；只支持默认分支 | [GitHub 博文](https://github.blog/engineering/architecture-optimization/the-technology-behind-githubs-new-code-search/)，[GitHub 文档](https://docs.github.com/en/search-github/github-code-search/about-github-code-search) |
| livegrep（C++，后缀数组 + RE2） | 博文未给数据 | 博文未给数据 | 博文未讨论 | [Nelson Elhage 博文](https://blog.nelhage.com/2015/02/regular-expression-search-with-suffix-arrays/) |

- 精确性机制：trigram 查询只挑候选文件，然后 "run the full regular expression search against only those documents"（Russ Cox）。所以只要 (a) 最终用同一个正则引擎确认，(b) 每个被索引跳过的文件都回退扫描，结果就可以与 (1) 完全一致。
- 现成实现不满足 (b)：Sourcegraph 跳过 >1 MB、二进制、>20,000 个唯一 trigram、非 UTF-8 的文件，并明说 "the same query can return more results unindexed than indexed"；GitHub 跳过 >350 KiB 等文件。gitfar 若用索引，必须把"跳过列表"也作为扫描输入。
- 为什么大规模必须索引：GitHub 测得 grep 约 0.6 GB/s/核，以此推算对其全量语料 "saturate 2,048 CPU cores for 96 seconds to serve a single query"，约 0.01 QPS。对单个内核量级仓库这个规模问题小三到四个数量级，所以 v1 不需要。
- 维护：每次分支前进都要重建或增量索引；GitHub 全量重建约 36 小时，增量（delta）后约 18 小时、抓取量减少 50% 以上。Zoekt 按分支重建 shard。
- Rust 生态：没有找到成熟的开源 Zoekt 等价物。tantivy 是倒排全文检索库，没有"正则 → trigram 布尔查询"规划；crates.io 上的 [`moeix`（`ix`）](https://docs.rs/moeix)自称 trigram 代码检索，索引约为源码 2–3x，但 docs.rs 显示近期版本构建失败，未评估可用性。Blackbird 是 Rust 但闭源。若将来自研，参考 Russ Cox 文中的正则分析算法（为每个子表达式计算 emptyable/exact/prefix/suffix/match）与 Zoekt 的 branch mask 设计。

## 本地基准（git/git，HEAD 浅克隆）

环境：Intel Core Ultra 7 255H，16 逻辑核，git 2.53.0，ripgrep 14.1.1；4,857 个文件，50.3 MB 工作树，pack 13.6 MB；热缓存，5 次取平均。脚本保存在会话 scratch 目录，没有提交。原计划用内核仓库，但网络约 100 KB/s，放弃。

| 模式 | `git grep <ref>`（bare，多线程） | `git grep --threads=1` | `rg`（checkout，多线程） | `rg -j1` |
|---|---|---|---|---|
| `strbuf_addf` | 49 ms | 177 ms | 22 ms | 52 ms |
| `ERR_[A-Z]+\|die_errno` | 91 ms | 357 ms | 22 ms | 60 ms |
| `\w+_[a-z]+\(\w+, \w+\)` | 398 ms | 2673 ms | 33 ms | 92 ms |

- `git archive HEAD`（单线程解压全部 blob）168 ms，约 300 MB/s，可以看作单线程解压成本。字面量查询中 `git grep --threads=1` 的 177 ms 几乎全是解压。
- 两者在 `strbuf_addf` 上的匹配数一致（1163）。
- 解读：解压大约是工作树扫描的 3 倍成本，但可并行；复杂正则下 `git grep` 的劣势主要来自 git 的正则引擎，(1) 用 grep-searcher 替代后可以消除。按线性外推到约 1.4 GB 的内核树（推算未实测）：单线程解压约 5 s，16 线程约 0.3–0.5 s。

## 对 spec 的输入

- 存储：每个上游仓库一份 bare 镜像（gix 读，fetch 可用 gix 或 git CLI）。
- grep 原语：输入 ref → 解析为提交 SHA → 遍历 tree → 并行（rayon）读 blob → grep-searcher `search_slice`；结果带 SHA。
- 可选缓存（v1 之后，或 v1 中按需）：(a) 按 blob OID 缓存解压后内容/匹配结果；(b) 按树 SHA 物化不可变快照，供热 ref 使用。
- 并发控制：全局扫描线程池 + 每查询配额/超时 + 结果上限，避免数十个并发的全量扫描互相饿死。
- 索引（v2 候选）：trigram 仅作候选过滤，跳过列表回退扫描，按 blob OID 去重以覆盖多 ref。
