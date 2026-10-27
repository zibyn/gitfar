# gitfar

让智能体像检索本地代码一样检索远程 git 仓库，而无需把仓库克隆到本地。

## Language

**远程仓库**（Remote Repository）:
由 URL 标识的一个 git 仓库，是被检索的对象。
_Avoid_: 项目、代码库（指代仓库时）、repo 地址

**服务端**（Server）:
托管的 gitfar 服务，持有远程仓库的缓存并执行检索。
_Avoid_: 后端、代理、本地版本

**客户端**（Client）:
调用服务端的接入方，即 CLI 或 MCP 连接；自身不持有仓库数据。
_Avoid_: 本地端、agent（指代接入方时）

**检索原语**（Search Primitive）:
服务端对外暴露的单个只读操作，形状模仿智能体熟悉的本地命令（如 grep、read、log）。
_Avoid_: 工具、API、命令（泛指时）

**仓库凭证**（Repository Credential）:
服务端访问私有远程仓库所用的凭证，由管理员在服务端配置。
_Avoid_: token（与客户端访问令牌混淆）、密钥

**解析提交**（Resolved Commit）:
一次检索实际作用的 commit SHA，由请求中的 ref 解析而来，随结果一同返回。
_Avoid_: 版本、快照

**规范 URL**（Canonical URL）:
远程仓库的唯一标识形式 `host/path`，所有前缀匹配与缓存归属都以它为准。
_Avoid_: 原始 URL、clone 地址（指代标识时）

**允许列表**（Allowlist）:
管理员配置的一组规范 URL 前缀，决定服务端可以访问哪些远程仓库。
_Avoid_: 白名单、仓库列表

**客户端访问令牌**（Client Access Token）:
客户端向服务端证明身份的令牌，与仓库凭证互不相干。
_Avoid_: token（单独使用时）、API key、仓库凭证

**令牌范围**（Token Scope）:
一个客户端访问令牌可读的规范 URL 前缀集合；未声明时为全局。
_Avoid_: 权限组、ACL
