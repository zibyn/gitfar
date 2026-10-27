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

**拒绝列表**（Denylist）:
管理员配置的一组规范 URL 前缀，从允许列表中排除仓库；拒绝优先于允许。
_Avoid_: 黑名单

**客户端访问令牌**（Client Access Token）:
客户端向服务端证明身份的令牌，与仓库凭证互不相干。
_Avoid_: token（单独使用时）、API key、仓库凭证

**令牌标识**（Token ID）:
管理员为一个客户端访问令牌起的唯一名字；日志、指标、轮换与吊销都以它指代该令牌，从不使用令牌原文。
_Avoid_: 令牌名、key id、令牌前缀

**令牌范围**（Token Scope）:
一个客户端访问令牌可读的规范 URL 前缀集合；未声明时为全局。
_Avoid_: 权限组、ACL

**续读游标**（Continuation Cursor）:
截断结果附带的不透明标记；单独交回服务端，即可在同一解析提交上取得后续内容。
_Avoid_: 分页 token、offset、会话

**预热**（Warming）:
服务端在远程仓库被检索之前把它完整拉入缓存的过程；由预热列表或该仓库的首次请求触发。
_Avoid_: 冷启动（指过程时）、预加载、同步

**预热列表**（Warm List）:
管理员配置的一组规范 URL，服务端主动预热并保持新鲜，不等请求到来。
_Avoid_: 预加载列表、订阅

**淘汰**（Eviction）:
服务端为腾出磁盘把一个远程仓库整份移出缓存；被淘汰的仓库下次被请求时重新预热。
_Avoid_: 清理、删除（指代缓存移出时）、过期

**冷却**（Cooldown）:
上游 host 返回限流（429/503）后，服务端在一段时间内暂停对该 host 的所有 clone 与 fetch；期间已缓存仓库用旧解析提交回答。
_Avoid_: 熔断、封禁、限流（指服务端自身状态时）

**领域错误**（Domain Error）:
检索原语按 gitfar 语义返回的错误（如 `not_found`、`repo_warming`），带固定错误码；区别于鉴权失败、协议错误等传输层错误。
_Avoid_: 异常、业务错误
