# ADR-115: Desktop 单一服务器地址模型（local / remote 降级为地址推论）

**状态**：已实施
**日期**：2026-11-13
**决策者**：大鱼

**修订**：
- [ADR-114](./ADR-114-account-system-decoupled-from-network-topology.md) §5.1 之后 Desktop 侧遗留的「模式 + 地址双旋钮」连接设置
- ADR-055（Node 拓扑）/ ADR-087（remote 模式自动拉起本机 Node）中把 `local` / `remote` 当作**用户可选择部署模式**的措辞
- **UI 形态修订**（2026-11-13，交付后）：§2.9 —— 语义不变（地址仍是唯一设置、mode 仍是派生），但撤回「无 radio 的平铺 UI」，恢复 radio 分区 + 富下拉列表；登录页 / 首启页回到单卡片居中

**前置**：ADR-114（账号体系与网络拓扑解耦）、ADR-033（MQTT 实时事件）、ADR-059 §5.1（bootstrap 阶段就绪）

---

## 1. 问题

ADR-114 把「账号体系」从网络拓扑里摘出来了，但 Desktop 的**连接设置**仍留着同一类耦合：两个互相独立、都能被用户写、可以互相矛盾的旋钮。

```
gatewayMode（持久化：local | remote | relay）   ← 用户选
gatewayUrl （持久化：任意字符串）               ← 用户填
        ↓
Rust: 是否 spawn 子进程 / MQTT 用哪个 transport / 远程 ACL 走不走
        ↓
前端: 登录门、引导流程、设置卡片分别按 mode 分支渲染
```

这两个值没有一致性约束，于是出现真实故障链（实测复现）：

1. 用户在登录页连了一个远程 Gateway，输错用户名密码 → 登录失败。
2. 地址被记住，每次启动都默认往那个地址登录，登不上；**登录页没有切换地址的入口**，只能卸载重装 / 删 `.acowork`。
3. 用户停掉远程 Gateway，重启后选 `local`、点 `Start local`，仍然失败 —— 因为 `gatewayUrl` 还是远程地址，`spawn_gateway` 在 `127.0.0.1` 拉起了子进程，而所有探测都发往局域网主机。
4. 重置远程密码后重新登录成功，进设置页一看：**模式是 `local`，地址是远程地址**。

根因有四个，彼此独立：

| # | 根因 | 后果 |
|---|---|---|
| R1 | `gatewayMode` 与 `gatewayUrl` 两个持久化旋钮无一致性约束 | 可以长期停在自相矛盾的状态（R3 的现场） |
| R2 | 登录页 / 首启页只有表单，没有地址控件 | 地址选错即死锁：进不去、也退不出来 |
| R3 | 派生关系反了：`mode=local` 却不校验地址是否本机 | 在 `127.0.0.1` 拉起子进程，探测打到局域网 |
| R4 | SplashScreen 的 `gatewayReady` 是 `useState(false)`，一旦 `true` 就再也回不去 | boot pipeline 只能跑一次，失败后无重启路径 |

另外 `local` / `remote` 这个区分本身在 Desktop 语境里已经没有信息量：两者都是 **Gateway 直连**，唯一差别是地址是不是本机。用户真正要做的选择只有「连哪个服务器地址」，而 `localhost` 本来就该是列表里的一个条目，不是一种隐藏模式。

## 2. 决策

### 2.1 用户可见的连接设置只有一个：服务器地址

`gatewayUrl` 是唯一输入，`gatewayMode` 是**推论**，不是选择：

```
gatewayUrl ──▶ isLoopbackGatewayUrl(url) ? "local" : "remote"
      relayActive ──▶ "relay"（唯一显式开关，由专用入口设置）
```

判定收敛到一个纯模块 `apps/acowork-desktop/src/lib/gatewayUrl.ts`（无 store 依赖，boot 路径、store、地址选择器、设置页共用同一份判定）：

- `normalizeGatewayUrl(raw)`：trim + 去尾部 `/`，保证 store / Rust `base_url` / 历史 LRU 里是同一个字符串。
- `isLoopbackHost(host)`：`localhost`、`::1`、`0:0:0:0:0:0:0:1`、整个 `127.0.0.0/8`（`127.0.0.2` 也是本机）、以及严格 IPv4-mapped `::ffff:127.0.0.0/8`。`::ffff:192.168.x.x` 是真实 LAN 地址，不算 loopback。
- `isLoopbackGatewayUrl(url)`：URL 解析失败时退化为手工 host 提取，**fail-closed 为 remote**。
- `deriveGatewayMode(url, relayActive)`：relay 优先，否则按 loopback 判定。
- `gatewayAddressOptions(current, history, lastGoodUrl)`：去重顺序固定为 `localhost → 上次连通过 → 当前 → 历史`。**`localhost` 永远在列表里**，且是普通条目（标注「本机」），不是被隐藏的模式。

### 2.2 `gatewayMode` 改为派生只读，不再是持久化旋钮

- 删除 `DEFAULT_GATEWAY_MODE`、`setGatewayMode()`、`requestGatewayModeChange()`、`applyGatewayUrl()`、localStorage key `acowork-gateway-mode`。
- store 里保留 `gatewayMode` 字段，但它是**写入时计算的投影**，只有一个写入口 `commitGatewayUrl(raw, relayActive, notifyAuth)`；任何地址写入都经它重算 mode 并推送 `set_gateway_config`。
- 新增 action：`switchGateway(url)`（清会话 + 重启 boot gate）、`setRelayGateway(url)`、`adoptBootUrl(url)`（boot 后采纳 Rust 的真实地址，不重启）、`resetConnectionSettings()`、`recordGatewayUrl(url)`、状态 `lastGoodUrl` / `relayActive`。

### 2.3 relay 保留为独立显式入口

relay 不是「另一个地址」，而是**另一种 transport**（WSS MQTT + access-token CONNECT + 远程 ACL + TLS device domain），因此不进地址列表，由设置页一个专用输入框设置，是**唯一**能把模式置为 `relay` 的路径。

不变量（写进 store，不靠调用方自觉）：

```ts
// relay 只存在于 TLS device domain 之上。任何明文地址写入都会静默退出 relay，
// 于是 store 永远不可能持有 "relay + http://" 这个 Rust 会永久拒绝的组合
// （MQTT 死掉但 /health 仍然应答 —— 就是经典的假「已连接」）。
const relay = relayActive && isHttpsUrl(url);
```

`relay_mqtt_wss_url`（`commands/chat_mqtt.rs`）只接受 `https://`；`relay + http://` 会让 HTTP health 正常应答而 MQTT 永久被拒。这条不变量修掉了「在 relay 模式下从候选列表点了 http 地址」这条复发路径。

### 2.4 boot gate 必须可重启

`connectionStore`（`ready` + `epoch` + `restart()`）取代 `App.tsx` 里的 `useState(false)`。`App.tsx` 用 `key={bootEpoch}` 重挂 `SplashScreen`，`restart()` 清 `ready` 并 bump `epoch`，于是整条 boot pipeline（config push → spawn/adopt → listeners → MQTT）能重跑。

这是 R4 的修复，也是「退不出来」的直接解法：地址选错不再是终局，而是一次可以重来的启动。

### 2.5 地址控件挂到每一个可能卡住的地方

`GatewayAddressPicker`（唯一地址控件，`UrlComboBox` + 可达性圆点 + 「本机」/「上次连通过」标记 + 可选重置）挂在五处：

| 界面 | 为什么必须在这里 |
|---|---|
| `LoginView` | 原故障现场：登录失败要说得出「换一个地址」，而不是只报密码错 |
| `SetupRequiredView` | 首启 setup 被远端 Gateway 403 `setup_local_only` 拒绝（ADR-114 §2.3）时，必须能切到 localhost |
| `SplashScreen`（加载中候选 / 超时恢复） | 连不上时唯一的逃生口 |
| `SettingsPage` | 连上之后换服务器 |
| `OnboardingFlow` 第 2 步 | 引导不能给一套与其它入口不同的设置方式 |

### 2.6 记住地址的时机：只在真正连通的那一刻

`recordGatewayUrl()` 只在 `gateway.status` **进入 `connected` 的上升沿**触发（`App.tsx` 顶层订阅）。输入过但没连通的地址不进历史；**登录失败（密码错）不得把地址从历史删除** —— 密码错 ≠ 地址坏，删掉只会让用户在「地址是对的」这件事上被误导。同时把该地址记为 `lastGoodUrl`，在列表里排到 localhost 之后的第一位，作为「回到可用状态」的默认路径。

### 2.7 选了 localhost 但本机没有 Gateway 在线 → 自动拉起

沿用 `spawn_gateway` 的 probe-then-spawn（先探活，能 adopt 就 adopt，否则 spawn 子进程并等 bootstrap READY）。新增的约束在 Rust 侧：

- **bind 与 advertise**（2026-11-13 复审修订）：子进程监听 `0.0.0.0:{GATEWAY_HTTP_PORT}` / `0.0.0.0:{GATEWAY_MQTT_PORT}`，`ACOWORK_GATEWAY_ADVERTISE_HOST` 固定为**默认路由接口**的 IPv4（`netinfo::default_route_ipv4`，UDP connect trick；无路由回落 127.0.0.1）。初版写死 loopback 是账号体系可选时代的姿态；ADR-114 之后账号恒开、首启 setup 仅限 loopback peer，LAN 暴露面已被鉴权兜住，而 loopback-only 会让配对二维码（携带 LAN IP）永远不可达 —— Desktop 自己连着 localhost、其他设备全部出局，正是本 ADR 要消灭的形态。**接口枚举序不可用于选 advertise 地址**（虚拟网卡排第一），必须用默认路由。
- **启动宽限**（2026-11-13 复审修订）：一次性 /health 探测失败 ≠ 端口占用者陈旧。独立启动的 Gateway（debug 构建）需数秒到数十秒才应答 /health；初版在探测失败后直接 `taskkill` 端口占用者，把启动中的 remote-mode Gateway 杀掉并换成 loopback 子进程（实测事故）。现在端口被占用但探测失败时先重试 ~8s，仍无应答才判陈旧。
- 如果用户配的是 loopback **别名**（`http://localhost:19876`、`http://[::1]:19876`）或**异端口**，子进程并不能服务那个地址，探测与后续所有请求都会打进黑洞 —— 又一种「进不去退不出」。因此 `adopt_spawned_base_url()`（`commands/gateway.rs`）在 `ownership == Owned` 时把 `base_url` 改回 `defaults::GATEWAY_HTTP_URL`（canonical loopback —— 0.0.0.0 绑定下它恒有效，且对未来换网免疫）并 warn；前端 `adoptBootUrl(boot.base_url)` 同步，保证持久化地址、探测地址、实际进程三者一致。

### 2.8 候选探测不再对 loopback 短路

`probeKnownCandidates()` 原本有 `if (gatewayMode === "local") return 0;` —— 这让 localhost 变成单行道：本机 Gateway 死了，就再也回不到用户之前用过的局域网 Gateway。现在 loopback 地址与其它地址一样从历史里取候选。

### 2.9 UI 形态修订：radio 分区 + 富下拉，撤回平铺（语义不变）

第一版实施把「删掉 mode radio」做成了「设置页 / 登录页全部平铺地址列表 + 设置页独立 relay 输入」，实测反馈两处形态问题：登录页被第二张地址卡片撑坏不再居中；下拉框下面又出现一份可点的平铺列表（同一份数据两套控件）。修订后：

- **`UrlComboBox` 是唯一列表**。新增 `renderOption?: (url) => ReactNode`，地址行（可达性圆点 + URL + 「当前 / 本机 / 上次连通过」标签、当前项高亮）全部渲染在下拉里；当前值不再从列表过滤掉。平铺 `<ul>` 删除。顺带修复一个真 bug：下拉选项点击时 `onCommit` 闭包里是未刷新的 `draft`（React setState 同步未生效），会把**旧地址**提交出去 —— `onCommit` 改为 `(url?: string) => void`，点选项时携带被选 URL。
- **设置页连接卡恢复 radio**：两选一（**直连 / 中继**），value 绑定**本地 `section` state**（初始 = 派生 mode，派生变化时同步）。radio 是 transport 分区（tab）语义，**从不写 mode**：点任何 radio 都只展开分区，store 只在分区内提交地址时变化。双旋钮死锁因此不会复活（mode 仍只从地址派生）。**radio 绝不能拆成本机 / 其他机器两个分区** —— localhost 和局域网地址合并进同一个地址列表正是本 ADR 的核心，拆回去等于把双旋钮 UI 复活（第一版 radio 化时犯过这个错，已纠正）。
- **分区内容**：直连 = `GatewayAddressPicker`（唯一地址列表，localhost 是普通条目）+ remote 时的 autoStart 开关；中继 = relay https 输入 + Device ID 行。本机 Gateway 的状态 / 起停在下方「本地网关」卡，一卡一事。
- **登录页 / 首启页回到单卡片**：`GatewayAddressPicker`（compact）并入表单底部 `border-t` 分隔区，删除堆叠的第二张卡片，恢复整体居中。
- i18n：新增 `connection.direct` / `connection.viaRelay`（5 语言）；删除孤儿 `settings.gatewayMode` / `settings.local` / `settings.remote` / `settings.relay`。

## 3. 接口变更

| 旧 | 新 |
|---|---|
| `DEFAULT_GATEWAY_MODE` | 删除 |
| localStorage `acowork-gateway-mode` | 删除 |
| `setGatewayMode()` / `requestGatewayModeChange()` / `applyGatewayUrl()` | 删除（无兼容分支） |
| `gatewayMode` 可由用户写 | 派生只读，仅 `commitGatewayUrl` 可写 |
| — | `switchGateway(url)` / `setRelayGateway(url)` / `adoptBootUrl(url)` / `resetConnectionSettings()` / `recordGatewayUrl(url)` |
| — | `lastGoodUrl`、`relayActive`；localStorage `acowork-gateway-url-history` / `acowork-gateway-last-good-url` / `acowork-gateway-relay-active` |
| — | `lib/gatewayUrl.ts`（`normalizeGatewayUrl` / `isLoopbackGatewayUrl` / `deriveGatewayMode` / `isHttpsUrl` / `gatewayAddressOptions`） |
| — | `components/connection/GatewayAddressPicker.tsx`，i18n `connection.*`（8 key × 5 语言） |
| SettingsPage 模式 radio + 条件 URL 区 | 连接 radio（直连 / 中继分区，§2.9）+ 直连分区内统一地址下拉 / 中继分区 relay 输入 + 常显诊断卡片 |
| OnboardingFlow 第 2 步模式 radio | `GatewayAddressPicker` + 状态行 + 测试连接 |
| GatewaySwitcherMenu 的 local/remote/relay tab | 统一地址列表（localhost 恒可点） |
| SplashScreen 模式下拉 + 条件地址框 | `GatewayAddressPicker` |

**Rust 侧 `GatewayMode` 枚举保留**：它仍决定 MQTT transport、publish SavePlan、relay ACL、owned 进程清理。变的只是它不再由用户书写、不再独立持久化，而是由前端从地址推导后下发。

## 4. 数据与迁移

项目未上线，代码不含兼容分支。旧 `acowork-gateway-mode` 键成为孤儿键，不会被读取；`acowork-gateway-url` 继续沿用，因此**升级后打开就会连上次那个地址** —— 如果它已经不可达，SplashScreen 的超时恢复页会给出地址列表，这是设计内的路径，不需要迁移脚本。

`gatewayUrl` 为空 / 非法时 `commitGatewayUrl` 回落到 `DEFAULT_GATEWAY_URL`（`http://127.0.0.1:19876`），因此 store 不可能处于「没有地址」的状态。

## 5. 验证

- `cargo check --lib`（`apps/acowork-desktop/src-tauri`）：通过（`adopt_spawned_base_url` 引入的编译期类型 / move 问题已修）。
- `npx tsc --noEmit`：无本次改动引入的错误；余下 9 项为既有环境缺失依赖（`docx-preview` / `pdfjs-dist` / `jszip` / `xlsx`，全部位于未改动的 `src/components/editor/`）。
- `node scripts/check-i18n.mjs`：OK，1940 处调用点（删除旧 `settings.local` / `settings.remote` / `settings.relay` / `settings.localModeKeepsUrl` / `settings.relayUrlSavedPlainHttp` 等 mode 文案后由 1962 下调）。
- vitest 全量：343 文件 / 3385 例，**3380 通过、5 失败**；5 例失败全部为既有问题，与本次改动无关（`GroupChatPanel.blockRef.test.tsx` 的 mention token 渲染，以及 4 个 `src/components/editor/*` 环境缺失依赖）。
- 本轮重写 / 新增的契约测试：
  - `settingsStore.modeSwitch.test.ts`（8 例）：地址派生 mode、loopback 别名（`localhost` / `127.0.0.2` / `[::1]`）均派生 `local`、切普通地址不留 relay、`switchGateway` 清会话并重启 boot、同地址重连、尾部斜杠归一、`resetConnectionSettings` 回 localhost、`recordGatewayUrl` 提升 last-good 且不改当前地址。
  - `settingsStore.switchGateway.test.ts`（6 例）：LAN → `remote` 推送、loopback → `local` 推送、relay 只能由 `setRelayGateway` 产生、relay 后切普通地址不会出现 relay+http、空地址忽略、每次 switch 只重启一次。
  - `GatewaySwitcherMenu.test.tsx`（19 例）：localhost 排第一、无 `combobox`、切行走 `switchGateway`、只禁用 probe 拒绝的远程地址、localhost 即使 probe 失败也可点、probe 不污染 `gatewayStore.candidates`。
  - `SplashScreen.test.tsx`（4 例）/ `SplashScreen.candidate.test.tsx`（11 例）：超时视图有地址选择器、localhost 是普通行、点 localhost 走 `switchGateway`（真正逃生口）、候选点击不再回滚 store、relay 下点 http 候选会丢掉 relay 并落到 `(remote, url)`。
  - `gatewayConnectivity.test.ts`：localhost 作为普通候选、从远端地址也能拿到 localhost 候选。
  - `lib/gatewayUrl.test.ts`（28 例，新增）：判定模块自己的单测 —— normalize、loopback 正反表（含两种 IPv4-mapped 写法）、`deriveGatewayMode`、scheme 校验、地址列表的顺序/去重/过滤。
  - `gatewayAutoStartLabel.test.tsx` / `gatewayGwId.test.tsx`：随设置页重构改为按地址（而非 mode）驱动；relay 入口的 TLS 校验改为内联 `role="alert"` + 禁用提交。

### 5.1 实施中自查发现并修掉的问题

| 级别 | 问题 | 修复 |
|---|---|---|
| P0 | relay 语义下从候选列表点 http 地址：`setGatewayUrl` 沿用 `relayActive` → 派生仍是 `relay` → 推送 `(relay, http://…)`，MQTT 永久拒绝而 `/health` 仍应答（假「已连接」复发） | 不变量下沉到 `commitGatewayUrl`：`relay = relayActive && isHttpsUrl(url)`；新增 `isHttpsUrl()` 导出与回归用例 |
| P1 | 初始化状态没过同一条不变量：旧构建留下的 `acowork-gateway-relay-active=true` 配一个 http 地址，启动即复活 relay+http | `initialRelayActive = getPersistedRelayActive() && isHttpsUrl(initialGatewayUrl)` |
| P1 | `setRelayGateway` 对非 https 地址静默降级（relay 被丢掉，实际连成 remote），调用方以为 relay 生效 | 非 https 直接拒绝（不推送、不重启）；设置页本就有 `role="alert"` + 禁用提交，两处一致 |
| P1 | `probeKnownCandidates()` 对 local 短路，localhost 成为单行道 | 删除短路，localhost 作为普通候选；测试改为「从 loopback 地址也会给出历史候选」 |
| P1 | spawn 出的 Gateway 只监听固定端口，用户配的 loopback 别名 / 异端口探测不到 | Rust `adopt_spawned_base_url()` + 前端 `adoptBootUrl()` 双向对齐真实地址 |
| P2 | `SettingsPage` 内本地重复实现 `isHttpsUrl`，与 `lib/gatewayUrl.ts` 两处口径 | 删除本地实现，统一 import |
| P2 | relay 输入框没有 `label`/`htmlFor` 关联，屏幕阅读器与测试都定位不到 | 补 `id="gateway-relay-input"` + `htmlFor` |
| P2 | 设置页 relay 提交按钮文案是「应用」，与地址控件的「连接」不一致（两者都是切换连接） | 统一为 `connection.connect` |

### 5.2 交付后逐文件 review 的追加修复

对整份 diff 逐文件复核（含新建模块是否有自己的单测），**确认为真问题并修复**：

| 级别 | 问题 | 修复 |
|---|---|---|
| P1 | `isLoopbackHost()` 漏判 IPv4-mapped 地址：`new URL("http://[::ffff:127.0.0.1]:19876").hostname` 被解析器**规范化成十六进制** `[::ffff:7f00:1]`，原正则只匹配点分形式 `::ffff:127.x` → 本机地址被判成 remote，于是不 spawn Gateway、`isGatewayLocal()` 为假，与 Rust 侧 `is_loopback_peer()` 的判定脱节 | 追加 `::ffff:7f00:–7fff:ffff` 十六进制映射形式；`::ffff:c0a8:10a`（192.168.1.10）等可路由映射地址仍判 remote；两个写法都进单测 |
| P1 | `lib/gatewayUrl.ts` 是 ADR-115 的唯一判定源，却**没有任何自己的单测** —— loopback 表、排序、去重全靠组件间接覆盖。上面那个 P1 正是这样漏掉的 | 新增 `src/lib/gatewayUrl.test.ts`（28 例）：normalize、loopback 正反表、`deriveGatewayMode`、scheme 校验、`gatewayAddressOptions` 的顺序/去重/过滤 |
| P1 | 无 scheme 的输入（`abc`、`192.168.1.5:19876`）被 `commitGatewayUrl` 接受并推给 Rust 成为 `base_url`，永久不可解析，且下次启动优先尝试它 | `gatewayUrl.ts` 新增 `hasHttpScheme()`；`commitGatewayUrl` 作为最终防线拒绝并 warn；`switchGateway` / `setRelayGateway` 用返回值决定是否重启；picker 的 Connect 禁用 + 内联 `role="alert"` |
| P2 | `GatewayAddressPicker` 的地址行不标记「当前」，与 `GatewaySwitcherMenu` 的 `current` 处理不一致 | 当前行加 `aria-current` + `listRowSelected` tint + `connection.current` 标签 |
| P2 | `SettingsPage` 诊断卡标题是 `settings.status`，卡内第一行标签又是 `settings.status`（「状态」卡里写「状态」） | 卡标题改为新增的 `settings.connectionDiagnostics`；同时把遗留变量名 `gatewayModeOpen` 改为 `gatewayConnectionOpen`，清理「shared between modes」过期注释 |
| P2 | `GatewaySwitcherMenu` 顶部约 40 行注释仍在描述已删除的 mode listbox、`applyGatewayUrl`、`setGatewayMode`、"returns 0 in local mode" —— 死注释，会误导后续维护 | 按现行实现重写：单地址列表、probe 白名单、`switchGateway` 语义，并明确写出「没有 mode 控件」及原因 |
| P2 | 删空值守卫后，`switchGateway("   ")` 会落到 `commitGatewayUrl` 的 `|| DEFAULT_GATEWAY_URL` 兜底，静默切到 localhost | `switchGateway` 保留空值守卫（被新增用例当场抓到） |
| P2 | 启动状态行仍是硬编码英文 `"Starting local Gateway..."` / `"Connecting to Gateway..."`：既没有地址（而地址就是唯一的设置项），又把已不该存在的 `local` 概念塞回用户视野；同一屏的 `Waiting for services...` / `Preparing:` / 超时文案也都是硬编码英文，中文界面中英混排 | 状态行改为 `(key, params)` 结构（渲染期 `t()`，语言切换也正确），文案 `Starting Gateway: {{url}}` / `Connecting to Gateway: {{url}}`，并补齐 `waitingForServices` / `preparing` / `retryingConnection` / `timeoutNoResponse` / `bootFailed`；标题栏芯片的 `bannerLocalStarting` / `bannerLocalDown` 改名 `bannerStarting` / `bannerNotRunning` 并去掉 `local`；清掉 7 个随模式 UI 删除而失效的孤儿 key（`modeLabel` / `modeLocal` / `modeRemote` / `modeRelay` / `startLocal` / `gatewayAddress` / `candidateDetected`），`timeoutHint` 从「切换至本地网关」改为「在下方选择其他服务器地址」 |
| P2 | 状态行走动态 key（`t(status.key)`），而 `scripts/check-i18n.mjs` 只扫 `t("literal")`，这些键对检查脚本不可见 —— 拼错会在"坏掉的安装最先看到的那一屏"上显示原始 key 名 | key 收敛为 `SPLASH_STATUS_KEYS` 常量联合（拼错即 tsc 报错），并用 `SplashScreen.statusCopy.test.ts`（4 例）断言 5 个语言都解析得到、占位符一致、且文案里不再出现 local 语汇 |

### 5.3 UI 形态修订轮（§2.9）的验证与自查

- `npx tsc --noEmit`：无新增错误；`node scripts/check-i18n.mjs`：OK（调用点 1945，含 `connection.viaRemote` / `connection.viaRelay`；4 个 mode 孤儿 key 删除）。
- vitest 全量：344 文件 / 3389 例，3384 通过、5 失败，与修订前基线**完全一致**（3 个 editor 环境缺依赖 + `GroupChatPanel.blockRef` 2 例，均既有）。
- 同步更新的契约测试：`SplashScreen.test.tsx`（下拉先点「Show history」再取 localhost 选项行）、`gatewayGwId.test.tsx`（Device ID 行移入中继分区，新增「打开分区是纯 UI 动作、不写 store」断言）、`gatewayAutoStartLabel.test.tsx`（字号参照标签改为地址选择器的 `Server address`）。
- 自查发现并修掉：下拉选项点击提交旧地址（P1，`onCommit(url)` 携带被选值修复，`SplashScreen.test.tsx` 用例当场抓到）。


## 6. 后果

**收益**：连接设置从「两个可互相矛盾的旋钮」变成一个字符串；`mode=local + 地址是远程` 这种状态在类型层面无法再出现；地址选错、密码错、本机无 Gateway 三种失败都有明确出路（地址列表 / 回登录页 / 自动 spawn）；SplashScreen 的 boot pipeline 可重跑，不再有「退不出来」的终局。

**代价 / 注意**：
- store 里的 `gatewayMode` 是缓存投影，破坏 `commitGatewayUrl` 单一写入口就会重新出现不一致。测试里直接 `setState({ gatewayMode })` 只用于构造初始场景，生产代码不得再单独写该字段。
- 地址历史只在 `connected` 上升沿记录，因此**从未成功连过的地址不会出现在历史里**。这是有意的（避免把拼错的地址变成「快捷方式」），但也意味着第一次连新机器必须手输一次。
- `lastGoodUrl` 与「当前地址」可能不同：前者是最近一次真正连通的地址，后者是当前选择。列表里两者都可能出现并各自带标记，不要试图合并。
- relay 与普通地址切换会走两条不同路径（`setRelayGateway` / `switchGateway`）。切到普通地址一律清 `relayActive`，这是 §2.3 不变量的另一面 —— 不清理就会重新造出 relay+http。
- Mobile 端不在本次范围内：它的连接面是「连不连 relay」，没有多地址场景，仍按 ADR-114 的 `account_state` 语义工作。
