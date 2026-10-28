# Claude Code / Codex 当前使用的代码检索工具（2026-10）

接续 [#2](https://github.com/zibyn/gitfar/issues/2)（`transcript-search-ops.md`）。调研日期 2026-10-28。结论来源：Claude Code 官方 CHANGELOG、openai/codex 源码、当前会话 shell 实测，以及在本机对话记录上重跑 `transcript_search_ops.py`。第三方工具只引其官方仓库，效果数字均为厂商自报，未核实。

环境：Claude Code 2.1.312（Linux 原生构建），codex-cli 0.163.0；对话记录 949 份 Claude Code（`~/.claude/projects`）+ 786 份 Codex（`~/.codex`）。

## 结论速览

1. **Claude Code 不再有独立的 Grep/Glob 工具**（macOS/Linux 原生构建）：2.1.117 起替换为 Bash 里内置的 `ugrep` 与 `bfs`。模型照写 `grep`/`find`/`rg`，shell 函数把它们转到 Claude Code 二进制里的 ugrep / bfs / ripgrep。
2. **Codex 没有专门的检索工具**：全部走 `exec_command`；系统提示词要求优先用 `rg` / `rg --files`。语义检索只是 feature request（openai/codex#5181），未发布。
3. **LSP 工具存在但几乎不用**：Claude Code 可经 LSP 插件开启（跳转定义、查找引用等），需要本地 checkout 与语言服务器；本机记录 0 次调用。对远程检索不适用。
4. **语义检索是第三方插件**（mgrep、grepai），都要先建索引，与 spec §8「不做向量检索、不做索引」冲突。
5. **对 gitfar 的含义**：原语方向（精确匹配 + shell 习惯形状）与两家一致，不需调整；但智能体的 `grep` 默认是 **BRE**，`'a\|b'` 在 gitfar（Rust regex）下静默变成字面竖线、0 匹配。已记为 [#48](https://github.com/zibyn/gitfar/issues/48)。

## 1. Claude Code

### 1.1 CHANGELOG 中的检索工具演进

| 版本 | 条目（摘录） |
|---|---|
| 1.0.84 | 默认使用内置 ripgrep（`USE_BUILTIN_RIPGREP=0` 关闭） |
| 2.0.30 | 移除自定义 ripgrep 配置 |
| 2.0.74 | 新增 LSP 工具：go-to-definition、find references、hover documentation |
| **2.1.117** | **Native builds on macOS and Linux: the `Glob` and `Grep` tools are replaced by embedded `bfs` and `ugrep` available through the Bash tool — faster searches without a separate tool round-trip (Windows and npm-installed builds unchanged)** |
| 2.1.162 | LSP `workspaceSymbol` 修复，接受 `query` |
| 2.1.275 | Grep/Glob 超过 20 MB 输出上限时挂起/OOM 的修复（Windows/npm 构建仍有这两个工具） |

### 1.2 shell 实测（2.1.312，bash）

`type grep find rg` 显示三者都是 Claude Code 注入的 shell 函数：

| 命令 | 实际执行 | 默认参数 |
|---|---|---|
| `grep` | ugrep 7.8.4（`exec -a ugrep <claude>`） | `-G --ignore-files --hidden -I --exclude-dir=.git …`（BRE、遵守 ignore 文件、含隐藏文件、跳过二进制） |
| `find` | bfs 4.1.1（`exec -a bfs <claude>`） | `-S dfs -regextype findutils-default` |
| `rg` | 内置 ripgrep（`exec -a rg <claude>`） | 无 |

带 `-Z`、`--null`、`--format-open` 等少数参数时 `grep` 回落到系统 grep。

### 1.3 本机记录

- 工具层：Bash 71.3%、Edit 14.3%、Read 8.1%；**Grep / Glob / LSP 0 次**。
- 命令段：`grep` 19.7%、`head` 13.6%、`sed -n` 8.7%、`cat` 3.2%、`ls` 2.3%。2.1.117 之后这里的 `grep` 实际就是 ugrep。

## 2. Codex

- 工具层：`exec_command` 64.3%、`apply_patch` 13.1%、`write_stdin` 8.2%、MCP 6.7%；无检索类工具。
- 系统提示词（`codex-rs/core/gpt_5_codex_prompt.md`，`gpt_5_2_prompt.md` 等同）：

  > When searching for text or files, prefer using `rg` or `rg --files` respectively because `rg` is much faster than alternatives like `grep`. (If the `rg` command is not found, then use alternatives.)

- `codex-rs/shell-command/src/parse_command.rs` 把 `rg`/`rga`/`fd`/`grep` 等命令归类为检索（用于界面展示与审批），不改变执行。
- 语义检索：[openai/codex#5181](https://github.com/openai/codex/issues/5181) 提议 `codex index` / `codex search`，未实现。
- 本机命令段排名与 #2 一致：`sed -n` + `rg` 为主。

## 3. 第三方语义检索

| 工具 | 方式 | 接入 | 备注 |
|---|---|---|---|
| [mixedbread-ai/mgrep](https://github.com/mixedbread-ai/mgrep) | 云端嵌入，自然语言检索代码/PDF/图片 | `mgrep install-claude-code`（skill + 提示词） | 自报 token 约减半；定位为 grep 的补充 |
| [yoanbernabeu/grepai](https://github.com/yoanbernabeu/grepai) | 本地 Ollama 或 OpenAI 嵌入，带调用关系追踪 | `grepai init/watch`，`grepai agent-setup` | 自报输入 token 大幅下降 |

两者都需要先建并持续维护索引，属于 spec §8 明确不做的方向。

## 4. BRE `\|` 的使用频率

ugrep `-G` 与 GNU grep 默认都是 BRE，`\|` 是「或」；gitfar 的 grep 用 Rust `regex`，`\|` 是字面 `|`。实测：

```
$ printf 'foo\nbar\na|b\n' > t.txt
$ grep 'foo\|bar' t.txt   # ugrep -G → foo, bar
$ rg 'foo\|bar' t.txt     # Rust regex → 无输出，exit 1；rg 'a\|b' 只匹配 "a|b"
```

本机记录中含 `\|` 的命令（正则直接扫原始 JSONL，可能重复计数，只看量级）：

| 命令 | 总数 | 含 `\|` | 占比 |
|---|---:|---:|---:|
| `grep` | 91,362 | 1,483 | 1.6% |
| `git grep` | 499 | 49 | 9.8% |
| `rg` | 34,331 | 15 | 0.04% |

`rg` 用户几乎不写 `\|`（rg 本身就是 Rust regex）；习惯 grep / git grep 的写法迁移到 gitfar 时会静默 0 匹配。

## 对 gitfar 的建议

- 维持现有原语与「输出与 git 精确一致」的目标；不引入语义检索或 LSP。
- [#48](https://github.com/zibyn/gitfar/issues/48)：pattern 含未转义的 `\|` 时报 `invalid_argument` 并提示用 `a|b`。
- [#44](https://github.com/zibyn/gitfar/issues/44) 的 skill：写明 gitfar 用 Rust regex（同 `rg`），不是 BRE。

## 来源

- [anthropics/claude-code CHANGELOG.md](https://github.com/anthropics/claude-code/blob/main/CHANGELOG.md)
- [Claude Code tools reference](https://code.claude.com/docs/zh-CN/tools-reference)
- [openai/codex `gpt_5_codex_prompt.md`](https://github.com/openai/codex/blob/main/codex-rs/core/gpt_5_codex_prompt.md)、[`parse_command.rs`](https://github.com/openai/codex/blob/main/codex-rs/shell-command/src/parse_command.rs)
- [openai/codex#5181](https://github.com/openai/codex/issues/5181)
- [mixedbread-ai/mgrep](https://github.com/mixedbread-ai/mgrep)、[yoanbernabeu/grepai](https://github.com/yoanbernabeu/grepai)
