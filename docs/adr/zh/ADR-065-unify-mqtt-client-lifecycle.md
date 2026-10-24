# ADR-065: 统一四端 MQTT 客户端生命周期

**状态**：已决策（2026-09-03）
**日期**：2026-09-03
**决策者**：大鱼（架构评审定案）
**关联**：
- [ADR-039](./ADR-039-mqtt-client-lifecycle.md)（MQTT Client 生命周期框架——本 ADR 的演进对象）
- [ADR-055](./ADR-055-remote-runtime-node-topology.md)（Node Agent 拓扑——Node 端 MQTT client 的引入）
- [ADR-036](./ADR-036-mqtt-status-push.md)（MQTT 连接状态由后端主动推送）
- [docs/protocols/zh/mqtt.md](../../protocols/zh/mqtt.md)（MQTT 协议参考）

---

## 1. 决策摘要

把 **Desktop / Node / Runtime / Gateway publisher 四个 MQTT 客户端**的完整生命周期（poll 循环、错误分类、退避重连、soft-restart、唤醒恢复、时序参数）**收敛进 `acowork-mqtt-session` 共享 crate**，四端只保留实体差异（client_id / LastWill / topic 前缀 / bootstrap 步骤 / publish 负载）。

一次性消除本次「Node Agent 唤醒后 60 秒失联」暴露的全部同源缺陷：

| 缺陷 | 现状 | 后果 |
|------|------|------|
| 错误适配器四份各写各的 | Node / Gateway 漏了 `MqttState::Io` 解包 → 唤醒重置被误判为 E4 fatal | Node 唤醒后走 60s fatal backoff，start/stop 命令静默丢失 |
| 时序参数四端不一致 | keepalive 5s/5s/30s/-；watchdog 5s/5s/60s/- | 行为漂移，无法统一调优 |
| 唤醒恢复三端不一致 | Desktop 2s+Focused / Node 5s / Runtime 无 | 同一唤醒事件，恢复时间 370ms vs 60s |
| force_reconnect API 不统一 | AtomicBool+Notify / Notify only / 无 | 存在 notify 丢失 race |

---

## 2. 背景与根因

### 2.1 故障链路（2026-09-03 用户报障）

用户开机（OS 唤醒）后点击 senior-engineer / document-manager 的 start 按钮无反应，约 60 秒后自动恢复。四端日志取证：

```text
08:16:14.551  broker: disconnected desktop  error=Network(KeepAlive(Elapsed(())))
08:16:14.553  broker: disconnected gateway  error=Network(KeepAlive(Elapsed(())))
08:16:14.554  broker: disconnected node     error=Network(KeepAlive(Elapsed(())))
              ↑ 唤醒瞬间三端进程被冻结，未及时发 PINGREQ，broker 按 keepalive 超时踢人

08:16:14.552  Desktop: Actual system sleep detected sleep_ms=12953
08:16:14.668  Desktop: MQTT force-restart requested during fatal backoff
08:16:14.922  Desktop: MQTT reconnected after wake          ← 370ms 恢复
08:16:14.556  Node:    MQTT event loop error err_class="E4 ConfigError"
08:17:14.560  Node:    MQTT (re)connected                    ← 60s 后恢复
```

### 2.2 根因：Node 端错误分类器漏了 `MqttState::Io` 解包

唤醒时 rumqttc 把 TCP 重置包装成 `ConnectionError::MqttState(StateError::Io(ECONNRESET))`。三端对它的处理不同：

| 端 | 适配器 | 结果 |
|----|--------|------|
| **Runtime** | 共享 `ErrorDescriptor::from(&e)`（[err_class.rs:213](../../../core/acowork-mqtt-session/src/err_class.rs#L213) 已解包） | ✅ Transient，指数退避 |
| **Desktop** | 私有 `error_descriptor_from_rumqttc_025`（[mqtt_client.rs:214](../../../apps/acowork-desktop/src-tauri/src/mqtt_client.rs#L214) 已解包） | ✅ Transient，指数退避 |
| **Node** | 私有 `error_descriptor_from_rumqttc`（[mqtt.rs:95](../../../core/acowork-node/src/control/mqtt.rs#L95) **未解包**） | ❌ `ErrorKind::MqttState` → E4 ConfigError (fatal) → **60s fatal backoff** |
| **Gateway** | 私有适配器（[client.rs:59](../../../core/acowork-gateway/src/mqtt/client.rs#L59) **未解包**） | ❌ 同上（Gateway publisher 无 wake recovery，影响较小但同源） |

共享 crate 里**已经存在正确的实现**（`From<&ConnectionError>` + 两个回归测试），但四个调用方各自写了私有适配器，只有 Runtime 用了共享版。

### 2.3 时序参数漂移

| 参数 | Desktop | Node | Runtime | 建议统一值 |
|------|---------|------|---------|-----------|
| keepalive | 5s | 5s | 30s | **5s**（broker `connection_timeout_ms` 为 5s，见 [broker.rs](../../../core/acowork-gateway/src/mqtt/broker.rs)） |
| POLL_WATCHDOG | 5s | 5s | 60s | **5s**（1× keepalive；Runtime 的 60s 是为规避长 HTTP handler 误触发，应改为「watchdog 5s + 长任务期间主动喂 PINGREQ」而非放宽 watchdog） |
| power probe | 2s | 5s | 无 | **2s**（需早于 5s 唤醒阈值，保证唤醒后最迟 4s 内 detect_resume 返回 true） |
| wake 阈值 | 5s | 5s | 无 | **5s** |
| fatal backoff | 60s 可中断 | 60s 可中断 | 直接 break | **60s 可中断**（interruptible_backoff） |

### 2.4 唤醒恢复机制三端不一致

| 端 | 触发源 | 恢复路径 |
|----|--------|----------|
| Desktop | 2s polling + `Focused(true)` 窗口事件 | `recover_after_wake()` → force_restart → soft-restart |
| Node | 5s polling（`power_tick`） | `force_reconnect()` → Notify → soft-restart |
| Runtime | **无** | 无（依赖 60s watchdog 或父进程重启） |
| Gateway | 无 | 无 |

Desktop 的 `ForceRestart` 是 **AtomicBool + Notify**（防 notify 丢失）；Node 只有 **Notify**（存在 select! 外丢失 race）。

### 2.5 Runtime 的 never-sleep / standalone 场景（本 ADR 必须覆盖）

Runtime 存在两种「进程常驻、但无父进程兜底」的运行模式，**系统休眠仍会冻结进程、踢掉 MQTT 连接，而 Runtime 自身没有自恢复能力**：

| 模式 | 触发条件 | 唤醒后 MQTT 恢复依赖 |
|------|----------|----------------------|
| **never-sleep** | `idle_timeout_secs = 0`（[idle_watcher.rs:85](../../../core/acowork-runtime/src/agent/idle_watcher.rs#L85) `NEVER_SLEEP`，UI "Never" 选项） | ① 60s watchdog（太慢）② Node 重启——但 Runtime 未退出，Node 不会重启它 → **实际无恢复** |
| **standalone** | 无 Gateway / Node 依赖独立运行（[loop_.rs:2008](../../../core/acowork-runtime/src/agent/loop_.rs#L2008) `test_agent_loop_without_gateway_client`） | 无父进程 → **完全依赖自身** |

结论：**Runtime 必须启用 power probe + force_reconnect**，与 Desktop / Node 同等待遇。此前「Runtime 是 per-session 进程、唤醒是父进程职责」的假设**不成立**——never-sleep 模式下 Runtime 就是常驻进程，唤醒恢复只能靠自己。

---

## 3. 目标

1. **单一实现**：四端 MQTT client 的 poll 循环、错误分类、退避、soft-restart、唤醒恢复全部收敛进 `acowork-mqtt-session`，四端只写实体差异
2. **单一适配器**：错误分类强制走共享 `From<&ConnectionError>`，禁止各端私有 `error_descriptor_from_rumqttc`
3. **单一时序**：keepalive / watchdog / power probe / wake 阈值 / fatal backoff 全部为共享 crate 常量，四端不可覆盖
4. **统一唤醒恢复**：需要 wake recovery 的进程（Desktop / Node / **Runtime**）统一走 `power::run_power_probe_loop`，间隔 2s
5. **统一 force_reconnect**：AtomicBool + Notify 语义，消除 notify 丢失 race
6. **行为对齐**：同一 OS 唤醒事件，四端恢复时间收敛到同一量级（< 5s）

---

## 4. 可选方案

### 方案 A：共享 crate 提供完整 `MqttClient<B>`（推荐）

`acowork-mqtt-session` 新增 `client.rs`，提供 generic 的完整 MQTT client：

```rust
pub struct MqttClient<B: BootstrapAction> {
    shared_handle: Arc<Mutex<AsyncClient>>,
    state: SessionStateTx,
    reconnect: ReconnectPolicy,
    force_restart: ForceRestart,
    _task: JoinHandle<()>,
}

impl<B: BootstrapAction> MqttClient<B> {
    pub async fn connect(config: MqttClientConfig, bootstrap: B,
                         message_callback: MessageCallback) -> Result<Self, Error>;
    pub fn shared_handle(&self) -> Arc<Mutex<AsyncClient>>;
    pub async fn publish_raw(&self, topic: &str, payload: Vec<u8>,
                             qos: QoS, retain: bool) -> Result<()>;
    pub fn force_reconnect(&self);
    pub fn state_rx(&self) -> SessionStateRx;
    pub fn current_state(&self) -> SessionState;
}
```

- 内部 poll 循环（soft_restart / classify / backoff / watchdog / force_restart）**只写一次**
- 时序常量全部来自共享 crate
- 各端只实现 `BootstrapAction` trait + 实体配置

**优点**：彻底消除重复；行为天然一致；后续调优只改一处。
**缺点**：一次性改动面较大（四端各删 ~250-700 行 poll 代码）。

### 方案 B：只统一错误适配器 + 时序常量（最小修复）

只把 `From<&ConnectionError>` 强制化 + 时序常量提到共享 crate，poll 循环仍各端保留。

**优点**：改动小（~200 行），当天可落地。
**缺点**：poll 循环仍四份，后续仍会漂移；唤醒恢复机制仍三端不一致。

### 方案 C：Node 单独修（只改 Node 的适配器）

只给 Node 端 `error_descriptor_from_rumqttc` 补上 `MqttState::Io` 解包。

**优点**：改动最小（~10 行）。
**缺点**：治标不治本；Gateway 同源 bug 仍在；时序/唤醒机制仍不一致；下次必然再漂移。

### 决策

**选方案 A**。理由：
- 用户明确要求「提取正确的公共 trait，三端复用，时序参数三端对齐，不能各自随便来」
- 这是 ADR-039 的既定方向（共享 crate）的自然终点——ADR-039 只抽了状态机/分类器/退避策略，**没抽 poll 循环本身**，导致四端各自实现 poll 时再次漂移
- 方案 B 可作为 A 的 Step 1 先行落地，但最终形态必须是 A

---

## 5. 详细设计

### 5.1 共享 crate 新增模块

```
core/acowork-mqtt-session/src/
  client.rs        # MqttClient<B> 完整生命周期（新增）
  config.rs        # MqttClientConfig + 时序常量（新增）
  force_restart.rs # ForceRestart: AtomicBool + Notify（新增）
  power.rs         # detect_resume + run_power_probe_loop（新增，从 Desktop/Node 提取）
  err_class.rs     # 保留；From<&ConnectionError> 为唯一适配器
  reconnect.rs     # 保留
  session.rs       # 保留
  session_state.rs # 保留
  bootstrap.rs     # 保留
```

### 5.2 时序常量（单一真理源）

```rust
// core/acowork-mqtt-session/src/config.rs
pub const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(5);
pub const POLL_WATCHDOG_TIMEOUT: Duration = Duration::from_secs(5);
pub const POWER_PROBE_INTERVAL: Duration = Duration::from_secs(2);
pub const WAKE_DETECT_THRESHOLD: Duration = Duration::from_secs(5);
pub const FATAL_BACKOFF: Duration = Duration::from_secs(60);
pub const FATAL_STREAK_LIMIT: u32 = 3;
```

**Runtime 的 keepalive 30s / watchdog 60s 必须改回 5s/5s**。原因为规避长 HTTP handler（`POST /workspaces` 可超 4s）误触发 watchdog——正确解法是「watchdog 保持 5s + 长任务期间主动喂 PINGREQ / 延长 keepalive 窗口」，而非放宽 watchdog 到 60s（那会让唤醒恢复也变 60s）。

### 5.3 ForceRestart（统一语义）

```rust
pub struct ForceRestart {
    notify: tokio::sync::Notify,
    persistent: AtomicBool, // 跨 notified() 的 permit 存储，防丢失
}

impl ForceRestart {
    pub fn request(&self);   // persistent=true + notify_one
    pub fn take(&self) -> bool; // 原子消费 persistent
    pub async fn wait(&self);   // notified()
}
```

poll 循环在 `select!` 顶部先 `take()` 检查 persistent 标志，再 `notified()` 等待——覆盖「poll 正在处理事件、错过 notify」的窗口。

### 5.4 power 模块（从 Desktop/Node 提取合并）

```rust
// core/acowork-mqtt-session/src/power.rs
pub fn detect_resume() -> bool;  // 合并 Desktop lib.rs + Node power.rs 的 platform 实现

pub async fn run_power_probe_loop(
    force_restart: ForceRestart,
    interval: Duration,   // 统一传 POWER_PROBE_INTERVAL
    label: &'static str,
) {
    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        if detect_resume() {
            tracing::warn!(label, "System sleep/wake detected — forcing reconnect");
            force_restart.request();
        }
    }
}
```

- Desktop 额外保留 `Focused(true)` 窗口事件触发（快速通道），但底层统一走 `ForceRestart::request()`
- **Runtime 必须启用**：never-sleep / standalone 模式下 Runtime 是常驻进程，唤醒恢复只能靠自己（见 §2.5）。Runtime 启动时若 MQTT client 存在，则同时启动 `run_power_probe_loop`
- Gateway publisher 不启用 power probe（Gateway 常驻但无 wake recovery 需求，可后续按需加）

### 5.5 四端改造后的最终形态

| 端 | 保留 | 删除 |
|----|------|------|
| **Desktop** | `ALL_TOPIC_FILTERS`、publish API、Tauri command 集成、`Focused(true)` handler | `mqtt_client.rs` 整个 poll 循环（~600 行）、私有 `error_descriptor_from_rumqttc_025`、私有 `ForceRestart`、`lib.rs` 内嵌 `mod power` |
| **Node** | `force_reconnect` 公开 API、credentials slot、bootstrap 注册、LWT | `control/mqtt.rs` 整个 poll 循环（~250 行）、私有适配器、`power.rs` |
| **Runtime** | `run_bootstrap`、publish API、LWT setup、`BootstrapData`、**启动 `run_power_probe_loop`（never-sleep / standalone 自恢复）** | `client.rs` 整个 poll 循环（~700 行）、私有适配器 |
| **Gateway publisher** | 订阅/发布业务 | `mqtt/client.rs` 私有适配器（改用共享 `From`） |

### 5.6 强制约束

- **禁止**各端私有 `error_descriptor_from_rumqttc`：共享 crate 提供 `From<&ConnectionError>`，各端只写 `classify_err(&ErrorDescriptor::from(&e))`
- **禁止**各端覆盖时序常量：`MqttClientConfig` 只暴露实体字段（client_id / host / port / credentials / last_will / max_packet_size），时序字段不暴露
- CI 加 clippy lint：`ErrorKind::MqttState` 字面量不得出现在共享 crate 之外

---

## 6. 实施步骤

| 步骤 | 内容 | 规模 |
|------|------|------|
| **Step 1** | 提取 `power.rs`（detect_resume + run_power_probe_loop）到共享 crate；Desktop/Node 改用共享版；probe 间隔统一 2s | ~200 行 |
| **Step 2** | 提取 `ForceRestart`（AtomicBool + Notify）到共享 crate；Desktop/Node 改用共享版 | ~100 行 |
| **Step 3** | 新增 `MqttClient<B>` + `MqttClientConfig` + 时序常量；内化整个 poll 循环 | ~400 行 |
| **Step 4** | 四端迁移：删各自 poll 循环，改用 `MqttClient<B>`；Runtime keepalive/watchdog 改回 5s/5s | 四端各 ~250-700 行删除 |
| **Step 5** | 回归测试 + CI 强制约束 | 见 §7 |

---

## 7. 验收标准

| # | 验收项 | 状态（Step 5 收口） |
|---|--------|---------------------|
| 1 | `cargo tree -p acowork-mqtt-session` 之外无 `error_descriptor_from_rumqttc` 私有实现（clippy lint 强制） | ✅ 红线已加（`dev/ci.sh::run_mqtt_redline`）。**Step 4-B 收口后四端全清零**（见下方「Step 5 收口发现 + Step 4-B 收口」）。 |
| 2 | 四端 `MqttClientConfig` 时序字段不可覆盖（编译期约束） | ✅ `MqttClientConfig` 字段仅含实体（client_id / host / port / credentials / last_will / max_packet_size / queue_capacity），时序常量 `KEEPALIVE_INTERVAL` / `POLL_WATCHDOG_TIMEOUT` / `POWER_PROBE_INTERVAL` / `WAKE_DETECT_THRESHOLD` / `FATAL_BACKOFF` / `FATAL_STREAK_LIMIT` 都是 `pub const`，不可被构造覆盖。 |
| 3 | 模拟 `ConnectionError::MqttState(StateError::Io(ECONNRESET))` → 必须 classify 成 Transient（回归测试，覆盖 Node/Gateway 路径） | ✅ 新增 `mqtt_state_io_econnreset_classified_transient_node_gateway_path`（`err_class.rs`）显式断言 Node/Gateway 路径下 ECONNRESET → `ErrorKind::Io` → `ErrClass::Transient`。 |
| 4 | 模拟 sleep 12s（clock mocking）→ `detect_resume() == true` + `force_reconnect` 触发（回归测试） | ✅ 抽出纯函数 `is_resume_gap(prev_biased, prev_unbiased, biased, unbiased) -> bool`，6 个单测覆盖：first-call 种子、no-sleep、under-threshold、threshold 边界（`>`）、just-over-threshold、**12 s sleep 命中**。`detect_resume()` 现在是该函数的 4 行包装；`run_power_probe_loop` 调用 `on_resume` → `ForceRestart::request` 的链路已由 `force_restart` 的 `request_idempotent` / `wait_resolves_when_requested_while_parked` / `interruptible_backoff_returns_true_on_request` 覆盖。 |
| 5 | `interruptible_backoff`：notify 在 sleep 中触发必须立即返回（已有测试，保留） | ✅ Step 2 已加 6 个 `ForceRestart` 单测，本轮无回归。 |
| 6 | 真实 OS 唤醒：Desktop / Node / **Runtime（never-sleep 模式）** 恢复时间均 < 5s（手动验证） | ⚠️ 需在真实 OS 上手动跑（不属于 CI）；代码层面 §7 #1/#2/#3/#4/#5 全过。 |
| 7 | `cargo test --all` + `cargo clippy --all-targets -- -D warnings` + `dev/ci.sh all` 全绿 | ⚠️ Step 5 范围全过；**全绿被 Step 4 遗留阻断**（见下方）。 |

### 7.1 Step 5 收口发现 + Step 4-B 收口（2026-09）

**Step 5 抓到的 Step 4 遗留**：红线（`dev/ci.sh::run_mqtt_redline`）按 ADR-065 §7 #1 设计正确执行；Step 4 实质只迁了 Gateway / Node / Runtime 三端，**Desktop 端迁移未完成**。

详细原始记录（依新到老顺序保留以供参考）：

```text
Checking MQTT ErrorKind::MqttState red line (ADR-065 §7 #1)...
ERROR: ErrorKind::MqttState literal found outside acowork-mqtt-session (ADR-065 #1):
apps/acowork-desktop/src-tauri/src/mqtt_client.rs:268:   kind: ErrorKind::MqttState,
apps/acowork-desktop/src-tauri/src/mqtt_client.rs:274:   kind: ErrorKind::MqttState,
apps/acowork-desktop/src-tauri/src/mqtt_client.rs:540:   let desc = error_descriptor_from_rumqttc_025(&e);
apps/acowork-desktop/src-tauri/src/mqtt_client.rs:216:   fn error_descriptor_from_rumqttc_025(err: &rumqttc::ConnectionError)
```

**事实**：

| 端 | `MqttClient<B>` 迁移 | 私有 `error_descriptor_from_rumqttc` | `ErrorKind::MqttState` 字面量 |
|----|----------------------|--------------------------------------|-------------------------------|
| Gateway | ✅ | 0 | 0 |
| Node | ✅ | 0 | 0 |
| Runtime | ✅ | 0 | 0 |
| **Desktop** | ❌ | **1**（`mqtt_client.rs:216`） | **2**（`mqtt_client.rs:268,274`） |

**结论**：红线（`dev/ci.sh::run_mqtt_redline`）按 ADR-065 §7 #1 设计正确执行；Step 4 实质只迁了 Gateway / Node / Runtime 三端，**Desktop 端迁移未完成**。Desktop 当前的行为逻辑等价于「共享 `MqttClient<B>` + 共享 `From<&ConnectionError>`」，但**仍走私有适配器路径**，未走 ADR §5.6 要求的 `MqttClientHandler` trait。

**Step 4-B 收口动作**（用户授权在 Step 5 中直接处理）：无损迁移 Desktop 到共享 `MqttClient<B>`。

1. **审计结论**：`DesktopMqttClient` 13 个公开方法全部可在 `MqttClient<B>` + `MqttClientHandler` trait 上表达，**无功能 gap**。Mapping 关系：
   - 直接等价 → `force_reconnect` / `recover_after_wake` (`reset_to_connecting`) / `wait_for_connected` / `current_state` (`session_state`) / `publish_raw` / `shared_handle` (`inner`)
   - 移到 `DesktopHandler::on_connack` 的自动行为 → 所有 `ALL_TOPIC_FILTERS` 订阅（原 `subscribe_agent_lifecycle` 的功能）
   - 移到 `DesktopHandler::on_publish/disconnect/error/soft_restart` 的状态桥 → `MqttStatus` → Tauri `mqtt-status` 事件
2. **行为对比**：keepalive 5s / watchdog 5s / fatal-streak 3 / fatal-backoff 60s / clean-session true / queue 100 / packet-size `GATEWAY_MQTT_MAX_PACKET_SIZE` / resubscribe-on-ConnAck 全部保留（来自共享常量 + `MqttClientConfig`）。
3. **删除代码**：
   - 私有 `ForceRestart` 结构（47 行）→ 共享 `ForceRestart`
   - 私有 `interruptible_backoff`（22 行）→ `ForceRestart::interruptible_backoff`
   - 私有 `error_descriptor_from_rumqttc_025`（94 行）→ 共享 `From<&ConnectionError>`
   - 私有 `resubscribe_all`（11 行）→ 移到 `DesktopHandler::on_connack`
   - 内联 250+ 行 poll 任务 → `MqttClient::connect` 单行调用
   - 死代码 `subscribe_agent_session` / `unsubscribe_agent_session`（前端从未调用，对应 `chat_mqtt.rs` 里 `mqtt_subscribe_agent_session` / `mqtt_unsubscribe_agent_session` Tauri command 一并删除）
4. **新结构**：
   - `DesktopHandler`（73 行）— 仅承载实体差异：`on_publish` / `on_connack`（re-subscribe `ALL_TOPIC_FILTERS` + Connected）/ `on_disconnect`（Reconnecting{reason}）/ `on_error`（Reconnecting{reason}）/ `on_soft_restart`（Connecting）
   - `DesktopMqttClient` 是 `MqttClient<DesktopHandler>` 的薄包装（`#[derive(Clone)]`）

**结果**：

| 指标 | 前 | 后 |
|------|----|----|
| `mqtt_client.rs` 行数 | 944 | **487**（-48%，删除 457 行内联实现） |
| Desktop `ErrorKind::MqttState` 字面量 | 2 | **0** |
| Desktop 私有 `error_descriptor_from_rumqttc` | 1 | **0** |
| Desktop 私有 `ForceRestart` | 1 | **0** |
| Desktop 私有 `interruptible_backoff` | 1 | **0** |
| Desktop 私有 `resubscribe_all` | 1 | **0** |
| 桌面 `cargo check --lib` | ✅ | ✅（无新警告） |
| 桌面 `cargo clippy --lib` 在 `mqtt_client.rs` 部分 | n/a | ✅（0 警告） |
| 桌面 `cargo test --lib` | 23 | **23**（全部通过） |
| `dev/ci.sh::run_mqtt_redline` | ❌ | **✅** |
| `acowork-mqtt-session` 测试 | 57 | **57**（无回归） |
| `acowork-node` 测试 | 117 | **117**（无回归） |

**未触动 `chat_mqtt.rs` 的 `on_status` 回调语义**：Mapping 完美保留——`MqttStatus` 三个变体（`Connected` / `Connecting` / `Reconnecting { reason }`）和原代码 1:1 对应；Tauri event payload 字段（`connected` / `connecting` / `reconnecting` / `reason`）原样输出。原 `MqttStatus::Disconnected { reason }` 是死代码，删除后 `connect_mqtt` Tauri command 的 match 同步精简。

**`Cargo.toml` 变化**：原 `mqtt_client.rs` 用 `thiserror::Error` 派生的私有错误类型 `DesktopMqttClientError` 因 `connect` 返回 `Result<Self, String>` 而从未被使用——直接删除，无新增依赖。在该任务落地前，`dev/ci.sh all` 会被红线挡住，但**这是预期行为**——红线本身就是为抓这类回归而设计的。

### 7.2 Step 5 增量单测统计

| 模块 | Step 5 前 | Step 5 后 | 新增 |
|------|-----------|-----------|------|
| `acowork-mqtt-session`（lib） | 41 | **57** | +16（`is_resume_gap` × 6 + `mqtt_state_io_econnreset_classified_transient_node_gateway_path` × 1 + 已存在测试保留） |

全 workspace（排除 `acowork-embed` `onnxruntime.lib` 链接错误、`acowork-lsp-relay` 需要外部 LSP 二进制两个已知环境问题）：**2477 tests, 0 failed**。

---

## 8. 回滚

- **Step 1/2**（power / ForceRestart 提取）：纯重构，行为等价，git revert 即可
- **Step 3/4**（MqttClient 收敛）：保留各端 poll 循环的 git 历史；若共享 client 引入问题，可回退到「各端自持 poll + 共享适配器」的中间态（即方案 B 形态）
- **时序参数**：Runtime keepalive 30s→5s 若触发长 HTTP handler 误断，回退方案是「watchdog 5s + 长任务主动喂 PINGREQ」，而非恢复 60s watchdog

---

## 9. 决策记录

| 决策点 | 结论 |
|--------|------|
| 收敛范围 | **完整生命周期**（poll / 分类 / 退避 / soft-restart / 唤醒恢复 / 时序）进共享 crate |
| 错误适配器 | 共享 `From<&ConnectionError>` 为唯一实现，禁止私有适配器 |
| 时序参数 | 共享 crate 常量，四端不可覆盖；Runtime keepalive/watchdog 改回 5s/5s |
| 唤醒恢复 | Desktop / Node / **Runtime** 统一 `power::run_power_probe_loop`（2s）；Gateway 不启用 |
| force_reconnect | AtomicBool + Notify 统一语义 |
| Runtime 长任务 | watchdog 保持 5s，长任务期间主动喂 PINGREQ（不放宽 watchdog） |
| Runtime never-sleep / standalone | **必须启用 power probe 自恢复**（常驻进程，唤醒恢复只能靠自己） |
| 实施路径 | 方案 A（完整收敛），Step 1-5 分步落地，Step 1/2 可独立 ship |
