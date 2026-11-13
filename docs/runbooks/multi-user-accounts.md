# 多用户账号系统 - 用户手册 / Runbook

> ⚠️ **本文的开篇已被 [ADR-114](../adr/zh/ADR-114-account-system-decoupled-from-network-topology.md) 取代。**
> `AUTH_MODE` / `auth_mode` / `--auth-mode` / `[multi_user]` 配置段**已全部删除**。
> 账号系统恒开，bind 地址不再决定登录模式；首次启动不再"拒启动"，而是进入
> `needs_setup` 受限态并从 Gateway 本机完成管理员设置。下文 §0–§2 已按 ADR-114
> 重写；§3 以后的账号生命周期 / 权限语义仍然成立，但凡出现 `AUTH_MODE`、
> `auth_mode`、按 bind 推断模式、"路由 404" 的段落都是**历史记录，不可照做**。
> 产品背景看 ADR-114；本文只讲"手怎么动"。

---

## 0. 一分钟版

```bash
# 什么都不用配：账号系统恒开，新装进入 needs_setup
acowork-gateway --daemon --home ~/.acowork/acowork-gateway
```

打开**和 Gateway 同一台机器**上的 Desktop，会出现首启向导（不是登录页）：
填用户名、显示名、密码 → 创建第一个管理员 → 直接带着 token 进主界面。

想跳过向导、用配置预置管理员：写 user 服务自己的 `acowork-user.toml`，
再在 `gateway.toml` 的 `[user].config` 指向它（见 §2.1）。

> 把 Gateway 从 `127.0.0.1` 改成 `0.0.0.0` **不会**改变任何账号语义，只是让
> 监听器能被网络上的其他机器连到。账号是一个还是多个，只取决于你建了几个。

---

## 1. 账号系统恒开：状态与网络拓扑无关

ADR-114 废除了 `AUTH_MODE`。现在只有三种**账号系统状态**，都来自
`acowork-user` 的 `accounts.json`，由 `GET /api/status.account_state` 下发：

| `account_state` | 含义 | Gateway 行为 |
|---|---|---|
| `uninitialized` | 账号存储不存在 / 为空 | 除 `/health`、`/api/status`、`POST /api/auth/setup` 外，全部 403 `setup_required` |
| `needs_setup` | 有账号但没有可登录的管理员（典型：启动时播种的无密码 `admin`） | 同上 |
| `ready` | 至少一个 admin 有真实 Argon2id hash 且未禁用 | 正常服务，无 token → 401 |

要点：

- **路由恒注册**：`/api/auth/*`、`/api/users/*`、用户聊天始终存在，不存在
  "local 模式 404" 这回事。首启是靠 `restricted_mode` 中间件返回 403 拦住的。
- **bind 只管连通性**：`[http].host` 是 `127.0.0.1` 还是 `0.0.0.0`，与账号
  系统是否存在、是否需要登录、是否已初始化**完全无关**。
- **首启只能本机做**：`POST /api/auth/setup` 由 Gateway 按**真实 peer 地址**
  （不是 bind、不是可伪造的 header）放行，且这个判定是无条件的，不受
  账号状态快照影响。relay 隧道进来的请求 peer 也是 loopback，所以 relay
  listener 直接把该路径 deny 掉。远端 Desktop / Mobile 会看到 403
  `setup_local_only`，需要回 Gateway 本机用 Desktop 或
  `acowork-gateway admin-setup` 完成。
- **会话可见性**：只看自己的；admin 可用 `?as_user=` 只读查看。新建会话
  默认私有（🔒）。`user_profiles.json` 是从 `accounts.json` 派生的展示视图。

> ⚠️ **暴露到非信任网络前必读**：`POST /api/auth/login` **没有速率限制，也没有账号 lockout**。
> 当前唯一的在线防线是密码强度（`password_policy`），其余靠部署面兜底。若要把
> Gateway 放到局域网 / 公网，**先**在它前面加一层限流（反向代理的 `limit_req` /
> fail2ban 之类）。这不是"可以留白的 ceiling"，是信任边界上的缺口。

---

## 2. 第一次启动（首启设置）

### 2.1 两条路，任选其一

**A. Desktop 向导（默认）**：什么都不配。user 服务在空的 `accounts.json` 上
播种一个无密码 `admin`，`/health` 报 `needs_setup`，Desktop 显示首启表单。
`POST /api/auth/setup` 原地激活这个 `admin`（不会建出第二个管理员），返回
token pair。重复调用返回 409。

**B. 配置预置管理员**：写 user 服务自己的 TOML（`acowork-user.toml`）：

```toml
[bootstrap_admin]
username = "root"
password = "换一个满足策略的密码1"     # 只在账号表为空时使用
display_name = "管理员"                # 可选

[password_policy]                       # 可选，下面是默认值
min_length = 8
require_digit = true
require_mixed_case = false

registration_open = false                # 见 §3.1
```

再在 `gateway.toml` 里把它交给 user 服务：

```toml
[user]
config = "/Users/you/.acowork/acowork-gateway/config/acowork-user.toml"
```

配了 `bootstrap_admin` 时，启动后 `/api/status.account_state` 直接是 `ready`，
Desktop 显示登录页，不出现首启向导。

> `bootstrap_admin` 是**一次性**的：账号表一旦非空，这项配置再也不会被读。
> 它不是"常驻的第二个管理员"，别指望改它来改密码——改密码走 Desktop（§4.2）。
> `UserServiceConfig` 带 `deny_unknown_fields`：残留的 `auth_mode` 键会**硬报错**，
> 这条报错是在提醒你配置还停留在 ADR-076 时代。

### 2.2 不再"拒启动"

旧行为（空账号表 + 没配 `bootstrap_admin` → 进程退出）已废除。现在进程正常
启动、端口正常监听、`/api/status` 如实报 `needs_setup`，其它 API 返回 403
`setup_required`，Desktop 直接给出可提交的首启表单。这样"没人能登录"是一个
**可自救**的状态，而不是一个要 SSH 上机器敲命令才能修的黑洞。

密码不满足策略时 setup 返回 422，响应体带上策略原文；Desktop 直接展示，
前端不再硬编码一份可能和服务端不一致的策略。

---

## 3. 账号生命周期

### 3.1 谁能建号

| `registration_open` | 非 admin 调 `POST /api/users` |
|---|---|
| `false`（默认） | 403。只有 admin 建号 |
| `true` | 可以自建，但**永远**是 `role = user`——请求里带 `role = "admin"` 也会被降级 |

注意 `POST /api/users` **始终需要已认证的调用者**：`registration_open = true` 的语义是"任何**已登录**账号都能再邀请一个账号"，不是"任何人都能注册"（匿名注册需要把这条路由移出认证中间件，是无认证攻击面的净扩张，刻意不做）。

Desktop 上这个开关的表现：开着时，`Users (N)` 分组顶部的 `+` 对**所有**账号可见；关着时只有 admin 看得到。开关状态由 `GET /api/status` 的 `registration_open` 字段下发——账号系统没启用时它恒为 `false`，所以不会出现"按钮在、点了 403"。

关着是默认值：让任意能连上的人自建账号，等于把"谁能用这台机"的决定权交出去。

### 3.2 两种建号方式

**方式 A：admin 直接给密码**（当面/可信渠道交付）

```bash
curl -X POST http://127.0.0.1:19876/api/users \
  -H "Authorization: Bearer $ADMIN_TOKEN" -H 'Content-Type: application/json' \
  -d '{"username":"alice","display_name":"Alice","password":"alice的密码1"}'
```

**方式 B：admin 只发邀请**（不给密码，让对方自己设）——推荐

不带 `password` 建号，响应里会返回一个 `invite_token`：

```bash
curl -X POST http://127.0.0.1:19876/api/users \
  -H "Authorization: Bearer $ADMIN_TOKEN" -H 'Content-Type: application/json' \
  -d '{"username":"bob","display_name":"Bob"}'
# → {"user_id":"...","invite_token":"<一次性 token>", ...}
```

把 `invite_token` 给对方，对方在 Desktop 登录页选"首次登录"（或直接 `POST /api/auth/first-login {invite_token, new_password}`）——设完密码即可登录。

邀请的规矩：

- **24 小时**过期（`INVITE_TTL_SECS = 24 * 3600`），过期的 token 一律拒绝
- **用后即焚**：激活成功即清空，重放同一个 token 会失败
- 时间戳非法 / 缺失 → **fail-closed**（拒绝），不会因为解析不了就放行

### 3.3 改密 / 重置 / 禁用 / 注销

| 操作 | 谁可以做 | 效果 |
|---|---|---|
| 改自己的密码 | 本人（`POST /api/auth/change-password`，要旧密码） | **杀掉该账号全部 refresh family** → 其他设备必须重新登录 |
| 重置他人密码 | admin（`POST /api/users/{id}/reset-password`） | 清空密码 + 铸新 `invite_token`（回落成"方式 B"）+ 杀全部 refresh family |
| 禁用账号 | admin（`POST /api/users/{id}/disable`） | 立刻不可登录；该账号的 refresh token 全撤销 |
| 注销自己 | 本人（`DELETE /api/users/{me}`） | 同上；历史 session **不删**，admin 仍可读 |
| 删除账号 | admin（`DELETE /api/users/{id}`） | 账号记录移除 |
| 登出（本设备） | 本人（`POST /api/auth/logout`） | 只杀本设备的 refresh family，不连坐其他设备 |

两条硬约束：

- **最后一个 admin 不能被 disable**（409）。否则这台机从此没人能建号。
- 禁用 / 注销的账号**立即**不出现在联系人名录里（§4.4），也不出现在 Desktop 的 Users 列表的正常位置上。

被禁用的账号有个刻意的小动作：密码被替换成 `$disabled$` 占位符，登录时**和密码错误返回完全相同的 401**——外部无法区分"这个用户不存在"、"被禁用了"、"密码写错了"。

---

## 4. Desktop 上怎么用

### 4.1 登录

`multi_user` 模式下 Desktop 启动就是登录页（`local` 模式不会出现这个界面，行为与以前一致）。

- 用户名 **大小写不敏感**
- 首次登录填 `invite_token` + 自设新密码
- access token 存在 `localStorage["acowork.auth.tokens"]`，过期后自动静默 refresh 一次并重放原请求；refresh 也失效 → 回登录页
- 所有 Gateway 请求由全局拦截器统一带 token，无需各处手动传

### 4.2 顶栏账号菜单（点右上角头像）

切换账号 / 修改密码 / 注销账号 / 用户偏好 / 退出登录。

"切换账号"和"退出登录"都会**重载窗口**——不是图省事：会话列表、聊天、未读计数全部挂在 token 身份上，逐个重置 store 漏一个就是数据串号，重载是唯一不会漏的做法。

### 4.3 侧栏 Users 分组

主界面左下、Agent 列表下方，一个默认**折叠**的 `Users (N)` 分组（只有 `multi_user` 模式出现）。

| 身份 | 看得到 |
|---|---|
| admin | 全部账号（含角色标签、禁用态置灰） |
| 普通用户 | 只有自己 |

admin 在该分组顶部 `+` 建号；右键某一行有：以该用户视角查看 session / 禁用 / 重置密码 / 删除 / 发消息。

普通用户通常看不到 `+`（建号是 admin 的事）；但若运维打开了 `[multi_user].registration_open = true`，他们的分组顶部也会出现 `+`——这是"邀请一位同事"的入口，建出来的账号恒为 `role = user`，且他们**看不到**自己创建的账号（普通用户的分组只列自己），凭证靠弹窗里的 `invite_token` 交付。

### 4.4 以某人视角看 session（`?as_user=`）

admin 右键"以该用户视角查看 session"→ 会话列表变成那个用户的可见范围，顶部有提示条和"清除"按钮。

**这是只读的**，三条线：

- 只有 admin 能设 `as_user`，普通用户设了 → 403
- admin 用 `as_user` 发**写**请求（POST / DELETE）→ 403，强制点在中间件里
- "写"永远按 token 的真实身份算，**不看** `as_user`

### 4.5 用户之间的聊天

顶栏"消息"图标进入收件箱，有未读时显示红点。

- 左侧会话列表（新消息在上），右侧线程，输入框 `Enter` 发送 / `Shift+Enter` 换行
- "新会话"按钮的联系人来自**联系人名录**：所有已登录用户可读，只返回 `user_id` / `username` / `display_name`，**排除已禁用账号**、**排除自己**
- 发送方身份**由服务端从 token 取**，请求体里的 `from` 一律忽略
- 读消息 = 自己或 admin；**发**消息 = 只有自己（admin 也不能代别人发）

附件：

| 类型 | 上限 |
|---|---|
| 图片（`image/*`） | 25 MiB |
| 其他文档 | 100 MiB |

超限 → 413。下载时 `Content-Type` 取自 Gateway 存储时记录的 mime（不信客户端声明），`Content-Disposition` 用 UTF-8 编码，中文文件名不会乱码。

### 4.6 隐私边界（名录端点为什么只回这么少字段）

`GET /api/users/directory` 是任何认证用户都能读的——否则普通用户无从知道"能发给谁"，只能求 admin 要 ID。为了让这条口子足够小：

- 只回 `user_id` / `username` / `display_name` / `avatar` / `builtin_avatar`，**不含**邮箱、时区、角色细节、自定义字段
- 排除已禁用账号
- 排除调用者自己

两个头像字段是展示状态，不是隐私：它们本来就出现在 `user_profiles.json`（从 `accounts.json` 重建、被 Runtime 当 `last_user_profile` 读），而 `GET /api/users/{id}/chats` 的 `peer_avatar` / `peer_builtin_avatar` 早就把它们发给了**任何**认证账号。名录端点回它们只是把既有暴露面拉平——不带的话侧栏只能画一个占位图标，而聊天面板（读 `peer_avatar`）显示真头像，同一个人两张脸。

**已知残余 ceiling**：`username` 全集仍然可被枚举（知道有几个人、都叫什么）。换来的是普通用户能自己发起会话。真要消除，需要"仅返回与我有过会话的人 + 精确 username 搜索"，那是另一轮设计（ADR-076 §5.5 有记）。

---

## 5. 数据落在哪（备份 / 迁移必读）

```
{HOME_DIR}/.acowork/acowork-gateway/
├── config/gateway.toml              # auth_mode + bootstrap_admin + 密码策略
└── data/
    ├── accounts.json                # 账号表（含 password_hash / invite hash）— 明文 JSON
    ├── auth/secret                  # JWT 签名密钥（HS256）— 删除 = 所有 token 立刻失效
    └── users/
        └── {min(A,B)}/chats/{max(A,B)}/
            ├── conversation.json    # 会话元数据（原子写：temp + rename）
            ├── messages.jsonl       # 消息，append-only
            └── files/
                ├── {attachment_id}          # 附件 blob（id = UUIDv4）
                └── {attachment_id}.json     # 元数据：filename / mime / size
```

三个名字要记住的约定：

- **配对目录 = min/max 排序**，与"谁先发起"无关。同一对人只会有一个目录（`chat_id = {min}__{max}`）。
- **`messages.jsonl` 是 append-only 的**：只追加、不原地改写，所以进程被 kill 也不会留下半个文件。读的时候逐行解析，单行损坏只 warn 并跳过——一条烂消息不会让整个会话读不出来。
- **附件与消息同树**：同一个会话的 blob 在它自己的目录里，天然按会话隔离，删会话 = 删目录。消息里存的是**附件 id 数组**，名字 / mime / 大小一律回查 `{id}.json`——客户端声明不了它没上传过的东西。
- **已知小块泄漏**：先写 blob 再写元数据，两次写之间崩溃会留下一个没人引用的孤儿文件。上传需要认证、窗口极窄，所以没有后台清扫线程；真要回收就按「没有 sidecar 且早于 N 天」扫 `files/`。

备份建议：`accounts.json` + `auth/secret` 一起备（单独备份前者没用——没有密钥就签不出 token）。**两者都是敏感数据**：`accounts.json` 是明文（password_hash 是 Argon2id，但用户名/角色/时间戳是明文），`auth/secret` 直接决定"能不能伪造任意人的 token"。文件权限建议 `0600`，别进 git。

---

## 6. 状态码语义（排查时先看这个）

| 码 | 含义 |
|---|---|
| 401 | 没 token / token 坏了 / 密码错 / 账号被禁用——**故意不可区分** |
| 403 | 身份合法但越权：非 admin 读别人的东西、admin 用 `as_user` 写、非 admin 建号（`registration_open = false`） |
| 404 | 权限模型里"读不到的东西一律 404"：session 不可读、不可写、`chat_id` 不合法、调用者不是会话参与者——**不泄漏"它存不存在"** |
| 409 | 用户名冲突；或"最后一个 admin 不能被禁用" |
| 413 | 附件超限（图片 25 MiB / 文档 100 MiB） |
| 5xx | Gateway 与 Node 不在一台机器时的 `install_path` 读取失败（见 ADR-009 §5） |

---

## 7. 故障排查

| 症状 | 原因 / 处理 |
|---|---|
| 启动即退出，stderr 提到 `bootstrap_admin` | `multi_user` + 空账号表 + 没配管理员。补 `[multi_user].bootstrap_admin`，或改 `--auth-mode local` |
| 启动即退出，提到 `password policy` | `bootstrap_admin.password` 不满足策略（默认 ≥8 位且含数字） |
| 启动成功但 Desktop 没有登录页 | 这台 Gateway 是 `local` 模式。看日志 `AUTH_MODE=local: single-user mode`；要账号系统就显式配 `auth_mode` |
| 登录页报"用户名或密码错误"，但你确定密码对 | 账号可能被禁用了——**故意与密码错误同码**，找 admin 确认 |
| `invite_token` 用不了 | 超过 24h 或已被用过（用后即焚）。让 admin 重置密码铸新的 |
| `GET /api/auth/login` → 404 | `local` 模式（路由未注册）。这是预期，不是 bug |
| 改了密码，另一台设备被踢 | 预期：改密杀掉全部 refresh family |
| 聊天附件上传 413 | 图片 >25 MiB 或文档 >100 MiB |
| 用户能登录但看不到任何 session | 正常：`multi_user` 只显示自己的。历史 session 的 `user_id` 为空时按"公开"处理 |
| 别人的账号看不到我刚建的会话 | **预期**：新建会话默认私有（🔒）。要共享就点输入框工具行的 🌐 开关 |
| 刚启动的 agent 里多出一个空会话 | 那是 agent **冷启动会话**：它先于任何账号存在，没有主人，因此被标为无人认领（除 admin 外谁都看不到）。每个账号第一次打开该 agent 时会自建一条属于自己的会话 |
| 想改一个"没有主人"的旧会话的可见性 → 403 | 只有 admin 能改无主会话的可见性：改它等于决定"谁能被共享"，而这属于所有者，无主会话没有所有者 |
| admin 用 `as_user` 想改点东西 → 403 | 设计如此：`as_user` 只读 |

---

## 8. 怎么验证这套东西没坏

```bash
cd core

# 单元 + router 级：模式真值表、token 轮换、越权、邀请生命周期、聊天权限
cargo test -p acowork-gateway --lib

# 进程级（ADR-076 §7.5）：真的起二进制，验拒启动 / 起得来 / local 不留痕
cargo test -p acowork-gateway --test auth_mode_e2e

# 回归防护（5 条红线，含"聊天路径只能在 chat.rs 里拼"）
bash dev/ci.sh check
```

`auth_mode_e2e` 的 4 个用例覆盖：`multi_user` 无管理员**拒启动**（退出码非 0 且 stderr 点名 `bootstrap_admin`）、`multi_user` 从 TOML 拿到管理员后**起得来**（端口可连 + `accounts.json` 落盘且明文不含密码）、`local` **不留痕**（无 `accounts.json`、无 `data/auth/`）、显式 `--auth-mode local` **压过 `0.0.0.0` 绑定**（这条是唯一会让服务裸奔的组合）。

> 仍未做：session 过滤的**跨进程** e2e（需要真起 Runtime + Node）。过滤判定本身（`SessionScope` 三态 / `is_readable_by` / 分页前过滤）已有单测，见 ADR-076 §7.1。
>
> **全部"仍未做"的清单**（有意留白 / 未实现 / 测试缺口 / 已否决，每项带触发条件）在 [ADR-076 §10 遗留清单](../adr/zh/ADR-076-multi-user-account-system.md#10-遗留清单仍未做全集)——想知道这套东西还有什么洞，看那一节就够了。
