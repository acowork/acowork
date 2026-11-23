# ADR-122：分块可续传上传 —— 统一二进制块通道，覆盖 user→agent、hosted-app 存盘与 agent→agent intent 附件

**状态**：已接受（2026-11-23；用户定案「不仅 hosted-app 存盘，还要包括 user→agent、agent→agent 的文件传递；intent 机制不带附件的要补上」。单文件默认上限 1 GiB 可配置。v1.1 同日评审修正：Q1–Q4 + Q2-bis 拍板定案，见 §7。）
**日期**：2026-11-23
**决策者**：用户
**修订**：
- **v1.1（2026-11-23，评审后修正）**——① D12 transfer store 归属钉死 **user 服务**（消除「Gateway/user 服务」双写含糊），引用校验补**创建者绑定**（`creator_instance_id`）；② §3 补契约细则：上限语义收口、输入校验、单写者与 complete 竞态、校验失败后 re-PUT 恢复、原子替换、声明值不当事实、客户端流式哈希、sweeper 归属；③ §4.3 补 response 方向 `attachments` 首版 MUST 忽略；④ §6 补双跳传输天花板（用户拍板接受，优化数据驱动）；⑤ 磁盘治理**不设 per-store 配额**，改走 [ADR-123](./ADR-123-gateway-unified-cache-management.md) Gateway 统一缓存管理（全局水位报警 + Desktop 弹框 + 缓存管理卡片）。
- v1.0（2026-11-23）——初版定案。

**关联**：
- [ADR-046](./ADR-046-unified-attachment-entries.md) / [ADR-040](./ADR-040-runtime-adapter-use-case-layer.md)（附件条目 union 与 Runtime use-case 层）
- [ADR-055](./ADR-055-remote-runtime-node-topology.md)（Gateway → Node → Runtime 三跳反代，单请求体 cap 每跳都在）
- [ADR-009](./ADR-009-gateway-workspace-isolation.md)（工作区文件只走 Runtime；跨 agent 不读私有 workspace）
- [ADR-121](./ADR-121-hosted-app-dual-canvas.md)（hosted-app 存盘 `PUT /workspaces/file`）
- [设计文档 27](../../design/zh/27-intent-messaging-and-group-chat.md)（intent 信封 / params 分工；群聊 `chat_attachment_ids` 引用 + node-auth 拉取模式，本 ADR 把它推广到 agent→agent）
- [ADR-112](./ADR-112-workspace-preview-capability-ticket.md)（能力票据先例：引用带授权坐标，不暴露裸路径）
- [ADR-123](./ADR-123-gateway-unified-cache-management.md)（Gateway 统一缓存管理：磁盘水位报警与各服务自报清理，v1.1 起承接本 ADR 的磁盘治理）

> 中文文档为 source of truth，英文 counterpart 按约定同 commit 维护。

---

## 1. 决策摘要

### 1.1 一句话

**所有大文件都走同一条固定大小原始二进制块通道；intent 与消息信封只带引用，字节由接收方经 node-auth HTTP 流式拉取。** 单请求体上限不再约束文件大小；进度与断点续传是分块的副产品。

### 1.2 关键决策表

| # | 决策 | 结论 |
|---|---|---|
| D1 | 传输编码 | 块用原始二进制（`application/octet-stream`）。**MUST NOT** base64：膨胀 4/3，64 MiB 体只装 48 MiB 文件 |
| D2 | 块大小 | 固定 **2 MiB**，MUST 小于链路最小单跳 body cap（当前各跳 64 MiB）。不做可配置块大小 |
| D3 | 接口语义 | 借 tus 最小四动词：`POST` 创建（总长+sha256）→ `PUT …/{offset}` 收块（幂等）→ `HEAD` 查 offset（续传）→ `POST …/complete` 流式合并校验。**不引 tus 库 / tusd** |
| D4 | 进度 | 已确认字节/总长；前端 `File.slice` 循环+回调，**不引 Uppy / tus-js-client** |
| D5 | 块落地与合并 | 块存临时目录 `.uploads/{uploadId}/{offset}`；complete 按 offset 升序**流式 append**，**MUST NOT** 整份读内存；校验总长+sha256 后才替换目标 |
| D6 | 上限解耦 | 单跳 body cap 保持 64 MiB 不动；单文件上限解耦、可配置、默认 **1 GiB**（取代 `MAX_UPLOAD_BYTES`，见 §3 上限语义）；磁盘是最终天花板，治理走 [ADR-123](./ADR-123-gateway-unified-cache-management.md) 统一缓存管理 + 全局水位报警（v1.1：不设 per-store 配额） |
| D7 | 范围 | **四条生产者全覆盖**：user→agent（单聊 composer 与群聊 mention 上传）、hosted-app 存盘、agent→agent intent 附件。统一同一分块契约，存储落点各异（§4） |
| D8 | 断点续传 | 保留 `HEAD` offset，跨会话续传 |
| D9 | 小文件不走分块 | <2 MiB 仍走现有单请求路由；**MUST NOT** 给小附件加三次往返 |
| D10 | 清理 | complete 成功 / 显式 abort / 超时 **24 h** 三者之一删块；合并失败保留块至超时 |
| D11 | intent 附件 = 引用不传字节 | `IntentRequest` 增 `attachments` 字段：已落 transfer store 的 `{id, 坐标, size, sha256}` 列表。**MUST NOT** 把字节放进 `params` 或 MQTT 信封（`GATEWAY_MQTT_MAX_PACKET_SIZE = 10 MiB`，`core/acowork-core/src/defaults.rs`） |
| D12 | transfer store 归属 | agent→agent 的字节落 **user 服务拥有的共享 transfer 区**（v1.1 钉死，消除「Gateway/user 服务」含糊：镜像群聊 `files/` 语义与「唯一写入口受信任服务」模式；Gateway 只反代不存储，守 Gateway 边界红线），不写发送方私有 workspace、不跨 agent 读私有 workspace（ADR-009）。transfer 记录 MUST 存 `creator_instance_id`，引用校验 = 创建者匹配。接收方经 node-auth HTTP 拉取 |
| D13 | 拉取 = 流式 GET | 接收方拉字节走响应流式 GET（复用 `download_chat_attachment` 形状的路由，`core/acowork-gateway/src/http/intent_api.rs`）。响应流不受请求体 cap 约束，**下载首版不分块**；升级路径 = Range |
| D14 | 实施顺序 | ① 统一分块服务 + user→agent；② hosted-app 存盘接同一契约；③ intent `attachments` + transfer store + 拉取路由（依赖①） |

## 2. 背景与问题

单请求上传把整份文件塞进一个 HTTP 体，文件大小被链路最小 body cap 卡死：Node 反代 `proxy_agent` 用 `Bytes` 缓冲全体、曾漏设 `DefaultBodyLimit` 吃 axum 默认 2 MiB（已补 64 MiB，`core/acowork-node/src/proxy/mod.rs`）；Gateway / Runtime 全局 `GLOBAL_BODY_LIMIT = 64 MiB`；附件用户上限 `MAX_UPLOAD_BYTES = 50 MiB`（`core/acowork-runtime/src/usecases/attachment.rs`）；hosted-app 存盘 base64 再膨胀 4/3。

四条生产者今天的状态：

- **user→agent 单聊**：composer multipart `POST /sessions/{sid}/files`，单请求、受上述 cap。
- **user→agent 群聊 mention**：已是引用模式——`attachment_ids` 指向群聊 `files/` 已上传件，Gateway pin 到共享 `user_message` 行，Runtime 经 node-auth HTTP 拉字节（设计 27 §4.2.1/§5.3）。上传侧仍是单请求。
- **hosted-app 存盘**：app→桥→宿主→`PUT /workspaces/file`（base64 JSON），单请求。
- **agent→agent intent**：`IntentRequest`（`core/acowork-gateway/src/http/intent_api.rs`）**没有附件字段**——只有 `params` / `repo` / `session_config` 等。群聊 mention 的引用模式没有推广到 intent。这是本 ADR 要补的洞。

抬 cap 只推迟问题；且单请求意味着每跳缓冲整份、失败重传整份、无续传。intent 若把字节塞 params 还会撞 MQTT 10 MiB 单包上限。分块 + 引用拉取一次解决。

## 3. 统一分块契约

四个动词，所有者按落点不同（Runtime 拥有附件/工作区落点；user 服务拥有 transfer 落点），Gateway / Node 透传：

| 动词 | 路径形状 | 体 / 头 | 答 |
|---|---|---|---|
| POST | `{store}/uploads` | JSON `{name, size, sha256}` | `{uploadId}` |
| PUT | `{store}/uploads/{uploadId}/{offset}` | 二进制块 ≤2 MiB | `{received}`；offset 不连续 409（首版顺序） |
| HEAD | `{store}/uploads/{uploadId}` | — | `Upload-Offset` |
| POST | `{store}/uploads/{uploadId}/complete` | — | 流式合并+校验 → 落目标，返回该落点的元数据 |
| DELETE | `{store}/uploads/{uploadId}` | — | abort 删块 |

`{store}` 取值：`/sessions/{sid}`（单聊附件）、群聊 session 的 files 区、`/workspaces`（hosted 存盘，complete 替换目标文件）、`/transfers`（agent→agent 共享区，**user 服务拥有**，D12）。块大小 2 MiB；重传同 offset 幂等；complete 前目标不存在/不被替换。

**v1.1 契约细则**（评审定案，MUST 级）：

- **上限语义**：`MAX_UPLOAD_BYTES`（50 MiB）由 per-store 可配置单文件上限取代（默认 1 GiB，D6），执行点在 complete 按声明总长与实收字节双重校验；旧常量废弃，不得与新上限并存。
- **输入校验**：`uploadId` MUST 服务端生成（客户端不可自选，防路径穿越进 `.uploads/`）；`{offset}` MUST 为非负整数且为块大小（2 MiB）整数倍，否则 400。
- **单写者与竞态**：一个 `uploadId` 同一时刻只接受一个写者——跨会话续传（D8）下第二个写者的 PUT MUST 409；complete 进行中到达的 PUT MUST 拒绝；complete 成功后重复调用 MUST 幂等返回同一元数据。
- **校验失败恢复**：complete 总长/sha256 校验失败时块保留（D10），`uploadId` 回到可 PUT 状态——客户端 MAY re-PUT 任意 offset 后重新 complete，不强制全量重传；per-chunk digest（精确定位坏块）留作升级项，与乱序并发块同批。
- **原子替换**：complete 落目标 MUST 经临时文件 + rename 原子替换，不得就地覆写。
- **声明值不当事实**：complete 校验通过前，客户端声明的 `size` / `sha256` MUST NOT 被任何消费方（intent 接收方、路由、UI）当作事实使用——与 ADR-103 v0.8「平台不得把关于端点的断言当配置事实直接下发」同一纪律。
- **客户端流式哈希**：创建时声明 sha256 意味着客户端先全量读一遍文件；实现 MUST 流式计算（`File.slice` 循环喂 hash），MUST NOT 整份读内存。
- **sweeper 归属**：Runtime 侧 `.uploads` 由 Runtime 清扫（启动 + 周期）；transfer 区由 user 服务清扫；均执行 D10 的 24 h TTL。

## 4. 四条生产者接入

### 4.1 user→agent（单聊 + 群聊）
composer / mention 上传 >2 MiB 改走 §3；<2 MiB 走现 multipart。群聊保持 `attachment_ids` 引用语义不变，只是上传侧换通道。Runtime 拉字节路径不变。

### 4.2 hosted-app 存盘
桥把 save 字节 slice 成块走 `/workspaces/uploads`；complete 流式替换目标工作区文件。宿主 `AppFrame` 不再一次性 base64 整份 PUT。 ArrayBuffer→Uint8Array 归一化保留（块本身就是二进制）。

### 4.3 agent→agent intent 附件（补洞）
- 发送方 Runtime 先经 §3 把字节落 `/transfers/uploads`（**user 服务**共享区，v1.1 钉死；Gateway 只反代），complete 返回 transfer id；transfer 记录 MUST 存 `creator_instance_id`。
- `intent_send` / `IntentRequest` 增 `attachments: [{id, size, sha256, name}]`（引用，D11）。Gateway 校验 id 存在且**发送方 = 创建者**（`creator_instance_id` 匹配，v1.1），pin 到 intent 行（行即接收方授权，与设计 27 §4.2.1 同构），转发进接收方执行轮。
- 接收方 Runtime 经 node-auth HTTP 流式 GET 拉字节（D13，复用 `download_chat_attachment` 形状的路由，按 transfer id + 接收方坐标鉴权），落自己 workspace 后按普通附件处理。
- 旧 Gateway / 旧 Runtime 缺 `attachments` 键时降级为「本轮无附件」（与设计 27 对 `chat_attachments` 的降级同构），不整条消息失败。
- **response 方向首版不带附件**（Q4 定案）：`kind = "response"` 的 intent 中 `attachments` 键 MUST 被忽略且不报错；将来放开是纯增量（字段已在信封上）。

### 4.4 小文件
四条生产者统一：<2 MiB 走各自现有单请求路由，不加分块往返（D9）。

## 5. 拒绝的替代方案

| 替代 | 拒绝理由 |
|---|---|
| 只抬 64 MiB cap | 不解决整份缓冲/整份重传/无续传；视频多图仍撞顶 |
| base64 分块 | 4/3 膨胀 + JSON 缓冲浪费 |
| intent params 带字节 | 撞 MQTT 10 MiB 单包上限；JSON 膨胀；信封应只带引用（设计 27 分工） |
| 跨 agent 直读发送方 workspace | 违 ADR-009 私有边界；需跨 agent ACL，比共享 transfer 区重 |
| 完整 tus / S3 multipart + 客户端库 | 依赖面与语义过重；只需四动词语义 |
| 可配置块大小 | 多配置面与校验分支，2 MiB 已远低于各跳 cap |
| 下载也分块（首版） | 响应流不受请求体 cap 约束，无必要；Range 留作升级 |

## 6. 后果与上限

- 收益：四条生产者文件大小与单请求 cap 解耦；Node 每跳只缓冲 2 MiB；失败重传一块；跨会话续传；intent 首次能带附件且不撞 MQTT 上限。
- 代价：大文件多往返（1 GiB ≈ 512 块）；服务端临时目录与清理责任；transfer 区多一个共享存储与鉴权面；complete 一次流式 IO。
- 已知天花板（首版 deliberate simplification，实现处以 `ponytail:` 标记）：顺序单连接上传（吞吐受 RTT 限制，升级=乱序并发块）；下载不分块（升级=Range）；无秒传（升级=sha256 索引）。
- **双跳传输天花板（v1.1，用户拍板接受）**：agent→agent 字节经 transfer store 中转，全程传两跳（发送方→store→接收方）。这是 store-and-forward 的固有代价，源于三个既定约束：intent 异步唤醒语义（发送时接收方可能未 spawn，字节必须落地等待）、ADR-055 hub 拓扑（跨机流量只能过 Gateway，无 Runtime 直达通道）、ADR-009 私有边界（只有接收方写自己 workspace）。单机部署两跳均为 loopback，开销 ≈ 2× 本地磁盘 IO，无网络放大；仅「跨机 + 大文件 + 高频」同时成立才有 2× WAN 放大。**定案：先按现方案实施，优化以实测数据驱动**——transfer store 埋点（跨机传输字节数、P95 拉取延迟），升级路径按序：sha256 秒传索引 → 乱序并发块 + Range →（数据证明必要时再议）Node 间直传。
- 回滚：分块路由与 intent `attachments` 均为新增；删除即回退单请求上传 / 无附件 intent。旧 multipart 与群聊引用路径未动，小文件与旧客户端不受影响。
## 7. 开放问题（v1.1 全部定案）

评审提出的 Q1–Q4 与 Q2-bis 已由用户拍板，本节为定案记录（格式对照 ADR-103 §10）：

| # | 问题 | 定案 |
|---|---|---|
| Q1 | transfer store 落哪个服务 | **user 服务**。三个独立证据：① 群聊 `files/` 先例——设计 27 §4.2.1「唯一写入口所在的受信任服务」；② 拉取路径今天已经过 user 服务——`download_chat_attachment` 鉴权后转发 `user_process` 端口（`core/acowork-gateway/src/http/intent_api.rs`），D13 复用该形状；③ Gateway 边界红线（通信+资源管理+反代，不持业务数据）排除 Gateway 存储，Node 本地则跨机拉取路径断裂（ADR-055） |
| Q2 | in-flight 配额策略 | **不设 per-store / per-owner 配额**（用户拍板：「单独给这个设限意义不大」——Gateway 上部署的多个服务都有磁盘占用，应统一治理）。磁盘治理升级为 Gateway 级统一缓存管理，见 [ADR-123](./ADR-123-gateway-unified-cache-management.md)：各服务自报占用、settings/gateway 缓存管理卡片、用户自行清理；transfer store 只保留 D10 TTL 自动回收 |
| Q2-bis | 全局磁盘水位 | **加，报警形态**（用户拍板）：水位（如 90%）触发时 Gateway 只报警到前端，Desktop 弹框提醒，用户点击弹框跳转 settings/gateway 缓存管理页面，自行决定清理哪些服务的缓存；**不做自动写入拒绝**。残留风险（报警到用户响应之间磁盘继续增长）显式接受，D10 TTL 为自动兜底。详见 ADR-123 D5 |
| Q3 | complete 校验失败恢复粒度 | **保留块 + re-PUT 任意 offset 后 re-complete**（§3 契约细则；D10 已保留块，只差契约一句），不强制全量重传；per-chunk digest 留升级项，与乱序并发块同批 |
| Q4 | response intent 是否带 attachments | **首版仅 request 方向**；response 的 `attachments` MUST 忽略且不报错（§4.3），将来放开为纯增量 |
