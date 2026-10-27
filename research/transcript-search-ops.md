# 智能体常用代码检索操作：Claude Code / Codex 对话记录统计

研究工单：[#2](https://github.com/zibyn/gitfar/issues/2)（地图 [#1](https://github.com/zibyn/gitfar/issues/1)）。

## 来源与方法

- 一手来源：本机 `~/.claude/projects/**/*.jsonl`（860 份，含子智能体）与 `~/.codex/**/*.jsonl`（785 份）。
- 脚本：[`research/transcript_search_ops.py`](./transcript_search_ops.py)（`--selftest` 自检；正式运行只输出聚合计数）。
  - Claude Code：解析 `message.content[].tool_use`，统计 Bash `command`、Read `offset/limit`、Grep/Glob 参数。
  - Codex：解析 `response_item` 中的 `function_call exec_command`（JSON 参数）以及 `custom_tool_call exec`（JS 代码里的 `tools.exec_command({cmd:...})`）。
  - Shell 命令用 `shlex`（引号感知）切分为 `&& || ; | 换行` 分隔的段，每段取首个程序名（剥离 `VAR=`、`timeout`、`sudo` 等前缀），记录其在管道中的位置与 flag。
- 隐私：只报告聚合计数与通用 flag 形态。数字统一归一化为 `N`；glob 只保留扩展名形态（如 `*.ts`），其余一律替换为 `<named-glob>` / `<dir-scoped-glob>`；出现次数 <3 的长尾值不输出。
- 局限：shell 切分是启发式的（子 shell、heredoc、`$(...)` 只做了近似处理），约 3.8% 的段归为“其他”（主要是 `ssh` 远程命令）。百分比用于看排序和量级，不是精确值。

## 结论速览

1. **检索/阅读占绝对多数**：在 38.8 万个 shell 命令段中，检索/阅读占 **64.8%**，构建/测试 9.0%，VCS 写 1.9%，文件写 0.9%（真正的写操作走 Edit 11.7k 次 / apply_patch 12.8k 次工具调用，不走 shell）。
2. **Claude Code 的 Grep/Glob 工具调用为 0**：所用版本的 harness 没有暴露这两个工具，检索全部走 Bash（`grep`/`rg`/`sed -n`），Read 工具只有 6.8k 次。因此 Grep 工具的参数组合（`output_mode`、`head_limit` 等）**无法从本机数据中测得**；下文用 shell 中等价的 flag 来代替。
3. **排名第一的原语是“按行区间读文件”**（`sed -n 'a,bp'` 6.9 万次 + `nl -ba` 7k + `cat` 9.3k + Read 6.8k），远超内容搜索。
4. **第二是内容搜索**（`grep` 4.7 万次，其中约 1.7 万次只是管道过滤器；`rg` 2.1 万次；`git grep` 约 600 次）。
5. **截断几乎无处不在**：`| head -N` 4.2 万次，是最常见的管道目标（占管道下游的 46%）。智能体是在客户端自己截断，服务端应提供原生的上限参数和截断标记。
6. **git 历史类操作量级中等**：`git diff` 9.1k、`git status` 5.9k、`git log` 5.5k、`git show` 2.7k（其中 32% 是 `rev:path` 形式，也就是“读某个修订版本的文件”）。`git blame` 极少（约 90 次，几乎都带 `-L`）。

## 1. 工具层调用量

| 来源 | 工具 | 次数 | 占比 |
|---|---|---:|---:|
| Claude Code | Bash | 57,501 | 71.0% |
| | Edit | 11,697 | 14.4% |
| | Read | 6,813 | 8.4% |
| | Write | 1,724 | 2.1% |
| | Grep / Glob | **0** | 0% |
| Codex | exec_command | 62,657 | 64.3% |
| | apply_patch | 12,787 | 13.1% |
| | write_stdin | 7,965 | 8.2% |
| | MCP（浏览器等） | 6,553 | 6.7% |

## 2. Shell 命令段分类（两个来源合计）

| 类别 | 段数 | 占比 |
|---|---:|---:|
| 检索/阅读（rg grep find ls cat sed -n head tail wc nl git log/show/diff/status/blame/grep…） | 251,563 | 64.8% |
| shell 杂项（cd echo printf for/if…） | 68,463 | 17.6% |
| 构建/测试（cargo pnpm npm docker python node…） | 34,865 | 9.0% |
| 其他（多为 ssh） | 14,807 | 3.8% |
| 网络（gh curl） | 7,518 | 1.9% |
| VCS 写（git add/commit/push…） | 7,313 | 1.9% |
| 文件写（rm mv cp mkdir sed -i…） | 3,516 | 0.9% |

按整条命令计：Codex 有 58.8% 的命令**只含**检索/阅读段（另有 8.2% 只多了 cd/echo 之类的杂项）；Claude Code 有 61.4% 的命令只含检索/阅读与杂项段。

## 3. 操作排名（命令段，合计）

| # | 操作 | 次数 | 作为管道首段 | 说明 |
|---:|---|---:|---:|---|
| 1 | `sed -n` | 68,847 | 56,251 | 行区间读取，Codex 首选（占其命令段的 30%） |
| 2 | `grep` | 46,645 | 29,882 | 约 36% 出现在管道下游（过滤输出） |
| 3 | `head` | 42,872 | — | 绝大多数是管道截断 |
| 4 | `rg` | 20,902 | 19,157 | Codex 首选搜索工具 |
| 5 | `tail` | 10,619 | — | 多用于截取构建/测试日志尾部 |
| 6 | `cat` | 9,311 | 8,966 | 94.8% 不带 flag |
| 7 | `git diff` | 9,144 | 9,144 | |
| 8 | `nl -ba` | 7,027 | 6,853 | 带行号的阅读，通常 `nl -ba f \| sed -n 'a,bp'` |
| 9 | `ls` | 6,187 | 6,187 | 78.8% 不带 flag，11% `-la` |
| 10 | `git status` | 5,885 | | |
| 11 | `git log` | 5,504 | | |
| 12 | `find` | 2,802 | | |
| 13 | `git show` | 2,740 | | |
| 14 | `wc -l` | ~2,400 | | |
| 15 | `git grep` | ~600 | | |
| 16 | `git blame` | ~90 | | |
| — | `fd` / `tree` | ≈0 / 4 | | 基本不用 |

两个智能体的偏好不同：Claude Code 以 `grep`（19.9%）+ `head` 为主，`sed -n` 只占 8.8%；Codex 以 `sed -n`（30.4%）+ `rg`（12.8%）为主，`grep` 只有 1.0%。

## 4. 典型参数组合

### 内容搜索：`rg`（flag 集合，共 20,902 次）

| 组合 | 占比 |
|---|---:|
| `-n` | 50.4% |
| `--glob … -n` | 13.3% |
| `-g … -n` | 10.9% |
| （无 flag） | 5.0% |
| `--files` | 3.1% |
| `-C N -n` | 2.7% |
| `--files -g …` | 2.3% |
| `-i -n` / `--glob -i -n` | 2.2% |
| `-S -n`（smart case） | 0.7% |
| `--hidden` 系列 | ~1% |

`rg` 的 glob 形态（共 11,788 个）：`!<目录>/**` 形式的排除 24.9%，`*.ts` 19.5%，指定文件名 13.9%，`!<文件名>` 排除 10.1%，`*.tsx` 6.0%，`*.md` 4.0%，`*.test.ts` 2.1%，`*.{ts,tsx}` 1.8%。也就是说 **glob 一半用于包含（按扩展名/文件名），一半用于排除（测试文件、锁文件、构建产物目录）**，`-g` 通常出现多次。

### 内容搜索：`grep`（flag 集合，共 46,645 次）

| 组合 | 占比 | 用途 |
|---|---:|---|
| `-n` | 21.9% | 多为单文件或管道 |
| `-v` | 14.6% | 管道过滤 |
| `-rn` | 11.7% | 递归代码搜索 |
| `--include=… -rn` | 11.6% | 递归 + 按扩展名过滤 |
| `-E` | 9.4% | 正则，多为管道过滤 |
| `-A N -n` / `-A N -E` / `-A N -B N -n` | ~9% | 上下文行 |
| `-c` / `-cE` | ~1.7% | 计数 |
| `-rln` / `--include -rln` | ~2% | 只要文件列表 |
| `-i` 系列 | ~3% | |

`--include` 的取值：`*.ts` 47%，`*.rs` 13%，`*.tsx` 11%，`*.md` 7%，`*.go` 7%。

### 文件枚举

- `find`：`-maxdepth N -type f/d` 20%，再加 `-print`/`-printf` 15%；`-name`/`-iname` 与 `-o` 组合、`-not -path` / `-prune` 用于排除。几乎都有深度限制。
- `rg --files [-g glob]` 约 1.1k 次；`ls` 大多不带 flag。

### 行区间读取

- `sed -n 'a,bp'` 的区间长度：≤40 行 16%，≤80 行 38%，**≤120 行 54%（中位数约 100–120 行）**，≤200 行 70%，≤300 行 92%，>500 行仅 2.8%。
- 有 43% 的区间从第 1 行开始（读文件开头），57% 从中间开始（通常是先 grep 出行号再跳过去）。
- Claude Read 工具：51% 用 `offset+limit`，43% 读整个文件；`limit` 的中位数约 60–80 行，≤200 行占 84%。
- `nl -ba` 共 7k 次、`cat -n` 4.6%：**智能体需要行号**，以便后续按行定位和编辑。

### git

| 子命令 | 常见 flag |
|---|---|
| `log` | `--oneline` 39%、`-N`（条数）34%、`-- <path>` 6.5%、`--format=` 5%、`--decorate`/`--all` 4%、`-S` pickaxe 0.8%、`--grep=` 0.3% |
| `show` | **`<rev>:<path>` 32.5%**、`--stat` 24.5%、`--oneline` 15%、`--format=` 8%、`--name-only` 1% |
| `diff` | `-- <path>` 34%、`--stat` 28%、`--check` 20%、`--name-only` 4%、`--cached` 4%、`--unified=N` 3%、`--numstat`/`--name-status` 2% |
| `grep` | `-n` 39%、`-- <pathspec>` 37%、`-i`/`-E` |
| `blame` | `-L a,b` 87% |

（`git diff --check`、`--cached` 与 `status` 都是针对工作区的，远程只读服务用不上。）

## 5. 输出截断习惯

- 管道下游的分布（9.06 万个非首段）：`head` 46.4%，`grep` 18.5%，`sed` 14.4%，`tail` 10.8%，`sort` 3.2%，`wc` 0.7%。
- `| head` 的 N：默认 10 行加 ≤10 共 27%，≤20 44%，**≤40 67%（中位数约 21–40 行）**，≤60 75%，≤300 再增加约 15%。
- `| tail` 的 N：≤10 占 52%，基本用于看构建/测试输出的结尾，不是检索。
- `2>/dev/null` 出现约 7.8k 次（静默“文件不存在”之类的错误），`2>&1` 约 13.8k 次（多见于构建/测试）。
- Codex 的 `exec_command` 有 62% 带 `max_output_tokens`（3.9 万次），中位数约 **16k tokens**，常用值为 12k / 30k / 20k / 10k；也几乎总是带 `workdir` 和 `yield_time_ms`。

## 6. 建议抽象为检索原语的操作族（按优先级）

| 原语 | 覆盖的现有操作 | 参数（来自上文的高频组合） | 默认值/上限建议 |
|---|---|---|---|
| **read** | `sed -n`、`nl -ba`、`cat`、`head`/`tail` 读文件、Read、`git show rev:path` | `path`, `rev`, `start_line`, `end_line`（或 `offset/limit`），输出带行号 | 未指定区间时返回前 ~200 行，并给出总行数和截断标记；单次上限约 500 行 |
| **grep** | `rg -n`、`grep -rn --include`、`git grep -n` | `pattern`、`fixed_string`/regex、`ignore_case`（含 smart-case）、`include_globs[]`、`exclude_globs[]`、`path`（目录范围）、`context`/`before`/`after`、`output: lines \| files \| count`、`max_results`、`rev` | 默认 `lines` 模式、始终带行号；默认上限 ~50 条匹配（对应 `head -40` 的习惯），返回总命中数和截断标记 |
| **list / find** | `rg --files -g`、`find -maxdepth -type -name`、`ls` | `path`、`glob`（可多个，可排除）、`max_depth`、`type: file\|dir`、`rev` | 默认深度 1（`ls` 语义）；递归时上限 ~200 条 |
| **log** | `git log --oneline -N -- path` | `rev`、`path`、`max_count`、`grep`（提交信息）、`pickaxe`（`-S`）、`since/until` | 默认单行格式、约 20 条 |
| **show / diff** | `git show --stat`、`git diff A..B --stat/-- path` | `rev`/`range`、`path`、`stat \| name_only \| patch`、`unified` | 默认 stat，patch 需显式请求并可截断 |
| **blame** | `git blame -L` | `path`、`rev`、`start_line`/`end_line` | 必须带行区间（数据显示几乎总带 `-L`） |
| （可选）**count** | `wc -l`、`grep -c` | 可并入 read（返回总行数）和 grep（`output: count`） | — |

不需要做成原语的：`grep -v`/`sort`/`uniq`/`cut`/`awk`（对输出做后处理，智能体会在客户端自己做）、`tail` 看日志、`git status`/`diff --cached`/`--check`（针对工作区）、`fd`/`tree`（基本不用）。

## 7. 对设计的启示

- 智能体的模式是“**grep 找到行号 → 按行区间读取 → 再 grep**”的循环。read 和 grep 两个原语应共享同一个 `rev` 解析结果，并且都输出行号。
- 截断由智能体自己完成（`| head`），但它不知道总量。服务端如果返回 `truncated: true` 加总数，可以省掉一轮“`wc -l` / 扩大 N 重试”。
- glob 的排除用得和包含一样多，而且一次常带多个，所以 `exclude_globs[]` 需要是一等参数。
- 搜索里扩展名过滤（`*.ts`、`*.rs`…）占大头，rg 的 `--type` 几乎不用（16 次），所以 glob 就够了，不必实现文件类型表。
