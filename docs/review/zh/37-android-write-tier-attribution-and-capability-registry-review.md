# 37 — Android 写档：归属门控执行点与能力词汇表，请求上游裁定

> **来源**：`ACoworkRuntimeAndroid`（Android 引擎移植，独立仓库）。本文件是**提交给上游评审的请求裁定文档**，不是上游已作出的裁定，也不修改上游任何代码。
> **涉及上游**：ADR-087 D1 / D2 / D5 / D7 / D8 / 备选方案 A；design 30 §6.2 三条关键裁定 / D12 / D14 / §9 表第 375 行；`core/acowork-core/src/permission.rs`。
> **涉及本地**：ADR-AND-001 §7.1（已就地标 ⚠️ 待对齐）/ §7.3 / §7.4。
> **状态**：等待裁定。**P0 阻塞项**：阻塞 Android 写档（P3）生产接线；只读档（P0/P1/P2）已交付并通过真机验证，不受影响。
> **日期**：2026-10-29

---

## 1. 请裁定的两件事（各一句话）

**B1 — 执行点冲突。** ADR-087 D5 把门控执行点定在 **Gateway 反代入口**，而本地 ADR-AND-001 §7.1 要求**放行决策始终发生在设备本地**。写档档的放行条件是「设备本地 dangerous-mode」**且**「请求者是 node owner」的合取，两个合取项分属两处权威，任何一处都求值不了完整合取式。请裁定执行点归属。

**B2 — 词汇表缺口。** 上游现有两张能力表：design 30 §6.2 的**引擎能力注册表**（点分、布尔、参数走 schema）与 `acowork-core::Permission`（冒号、**带路径/URL 参数**）。Android 要上报的核心事实是「**设备侧安全域**」（可执行命令集、可写路径前缀、探测所得执行档）——它既不是用户可设置项（D14 明确 schema 只描述用户可设置项），也不适合塞进布尔 ID（D12 明确避免无界 ID 空间）。**两张表都没有它的位置。** 请裁定它进哪张，或是否需要第三张。

---

## 2. B1：冲突的形式化

本地 ADR-AND-001 §7.4 写档放行条件 = 合取：

| 合取项 | 它代表的真相 | 真相住在哪 | 设备能否读到 | Gateway 能否读到 |
|---|---|---|---|---|
| `dangerous-mode` 已开 | 此刻本机是否允许写 | 设备本地 | ✅ | ❌（D1：不进 proto） |
| 请求者是 node owner | 归属 | `node_owners.json`（D1） | ❌（D1/D5 明文禁止） | ✅ |

```
在设备本地求值合取  ⟹  需要 D1 禁止下发的归属真相（= 已被拒绝的备选方案 A）
在 Gateway 求值合取 ⟹  需要 D1 禁止下发的 dangerous-mode 事实，违反 §7.1
```

两条规则**不能同时按字面成立**。这不是移植侧实现没做到，而是规则层缺口，所以提交上游而不是自行消化。

> 本地 ADR-AND-001 §7.1 已就地标注该冲突（⚠️「本节两处裁定已被上游推翻，待对齐」），不假装没看见。

---

## 3. 已采用的解法：两权威 + 单一合成点（已落地，可复核）

| 权威 | 回答的问题 | 持有者 | 到达设备的形式 |
|---|---|---|---|
| 控制面 | *谁*可以请求这个档 | Gateway（`node_owners.json`，D1 原样不动） | **不透明布尔裁决** `AttributionVerdict` |
| 设备 | 此刻*什么*是可行的 | 设备本地（dangerous-mode + 能力地板） | 本地事实 |
| 合成点 | 合取 | **设备本地** | `Gate::decide` |

Gateway 交出的只是一个布尔，不含 user id、不含角色、不含任何身份形状。因此：

- **D1 成立**：归属真相不下发、不进 proto；
- **D5 成立**：Node 不感知用户；
- **D8 的纪律成立且被复用**：D8 已裁定「服务端算好 `can_manage`/`can_use` **布尔**下发，客户端**只消费布尔、不再自行推导**」——`AttributionVerdict` 正是同一形状的产物，不是新发明；
- **§7.4 的意图成立**：合取仍在设备评估，设备保留最终否决权，默认无裁决即拒绝（fail-closed）。

**这与 D5 的关系需要上游确认**：D5 说执行点是 Gateway 反代入口。我们的解法把**归属判定**留在 Gateway（符合 D5），把**可行性判定与合取**放在设备（因为 dangerous-mode 只有设备知道）。也就是说：**对"设备本地安全档"这类门控，D5 的执行点需要限定为"Gateway 负责它知道的那一半"**。请确认或否决这一限定。

**落地证据**：
- 规则文本：`crates/acowork-platform-contract/src/lib.rs` 规则表 **R7**；
- 类型与合成点：`crates/acowork-platform-contract/src/gate.rs`（`AttributionVerdict` / `LocalReadiness` / `Gate::decide`）；
- 三条不变量测试：`r7_the_attribution_verdict_carries_no_identity`（裁决不含身份）、`r7_the_conjunction_happens_on_the_device_and_neither_authority_suffices`（任一权威单独不足以满足）、`r7_a_verdict_cannot_be_replayed_against_a_higher_tier`（低档裁决不可向上重放）。

---

## 4. 请上游明确回答的三个问题

**Q1 执行点**：设备本地安全档的合取，按 §3 的限定（Gateway 判归属、设备判可行、设备做合取）是否可接受？
若不可，请指出替代路径 —— 我们能看到的其他选项都不更好：
&nbsp;&nbsp;**b.** owner 表下发到 Node = D1 已拒绝的备选 A；
&nbsp;&nbsp;**c.** dangerous-mode 上报到 Gateway = D1 明文「不进 proto」；
&nbsp;&nbsp;**d.** 写档完全由 Gateway 审批、设备只执行 = 设备失去最终否决权，与 §7.4 的初衷相反。

**Q2 契约归属**：`AttributionVerdict` 是否应上升为**上游契约类型**（目前是本仓库契约层自定义），以便 Node 侧与设备侧共用同一形状、避免各引擎各自发明"布尔裁决"的线格式？

**Q3 无主节点**：D2 允许「CLI 签发的 token 无主 → 该 node 无主（admin-only）」，D7 裁定「无主 fail-closed，`owner=None` 只有 admin 可 manage」。那么一台无主 Android 节点：设备本地 dangerous-mode 已开、但永远不会有 owner 裁决 ——
&nbsp;&nbsp;当前实现是**永久拒绝写档**（无裁决即拒绝）。
&nbsp;&nbsp;按 D7 的 admin-only 语义，是否应改为「**admin 可走另一条授权路径下达裁决**」？请裁定 admin 路径在设备侧以什么形式呈现（仍是一个布尔裁决即可？还是需要区分 admin 裁决与 owner 裁决）。

---

## 5. B2：设备侧安全域事实，在上游两张表里都没有位置

上游现有两套词汇（**均已在代码/文档中核实**）：

| 表 | 位置 | 命名 | 形状 | 语义 |
|---|---|---|---|---|
| 引擎能力注册表 | design 30 §6.2 / D12 | 点分（`session.reasoning.effort`） | **布尔，不带参数**；参数化差异走 §6.3.2 schema | 引擎*能配置什么* |
| `Permission` | `core/acowork-core/src/permission.rs:112` | **冒号 + 参数**（`filesystem:write:~/workdir`、`intent:send:com.example.calendar`） | `FilesystemWrite(Option<String>)` 等 | Agent *被授权做什么* |

design 30 §9 表第 375 行点名：「ACoworkRuntimeAndroid 的 `HostProfile` 上报应成为**注册表**的一个来源」——按上下文指向前者（引擎能力注册表）。但 Android 侧真正需要上报的事实是：

| Android 事实 | 形状 | 进布尔注册表？ | 进 `Permission`？ |
|---|---|---|---|
| 可执行命令集（`getprop`/`settings get` 可读；`setprop`/`settings put` 属写档） | **集合** | ❌ 无界 | ⚠️ 形状像，但它不是 agent 声明，是设备事实 |
| 可写路径前缀（`/data/local/tmp`、app 私有目录） | **路径参数** | ❌ 违反 D12 | ⚠️ `FilesystemWrite(Option<path>)` 形状吻合 |
| 执行档（system-shell / root / APK 沙箱，探测所得） | **枚举事实** | ✅ 可拆成 `android.shell.root` 布尔 | — |
| 快照预算 / 分片上限 | 数值 | ❌ | — |

**关键错位**：D14 明确「参数 schema 只描述**用户可设置项**，不接管生效值来源」。而"可写路径前缀"**不是用户可设置项**，是设备安全边界 —— 用户能把它调宽，就等于让 UI 扩权。于是它既进不了布尔注册表（D12），也不该进参数 schema（D14），而 `Permission` 表的所有权又属于 agent manifest 而非 Node 上报。

**请裁定**（四选一）：
- **a.** 新增第三张表：**Node/设备安全域表**（`HostProfile` 上报的目标），带参数、冒号命名、由设备探测填充、**只读不可由 UI 扩宽**。← 我们推荐，语义最干净。
- **b.** 复用 `Permission`：允许 **Node**（而非 agent）上报 `Permission` 条目，语义扩为"该主机被授权的范围"。改动最小，但把 agent manifest 授权与设备事实混进同一张表。
- **c.** 只进布尔注册表：`android.shell.root` 这类拆成布尔；带参数的事实（路径前缀、命令集）**不进注册表**，只作设备本地地板（我们已实现 `check_declared`）。上游零改动，代价是 HostProfile 上报不完整、Gateway 无法区分"能调试的节点"与"普通手机节点"。
- **d.** 其他（请给出期望形状与命名规范）。

**顺带请裁定命名规范**：点分（design 30 §6.2）还是冒号（`Permission` 实际形态）？本地 ADR-AND-001 §7.3 目前用冒号（`android:shell:read`），若裁定为点分，移植侧统一改名（成本很低，但需要一次定论）。

---

## 6. 移植侧的纪律（缩小评审面）

- ❌ 不要求把 owner 真相放进 proto / 下发 Node（D1 保持原样）。
- ❌ 不要求"运行时自定义能力通道"——接受 §6.2 的注册表 PR 流程，Android 每新增一条能力就走一次上游。
- ❌ 不自造 wire 词汇：本地 `Capability` 是**领域类型**（带参数、冒号命名），到上游注册表的适配层**等裁定后再写**，成本一个文件。
- ❌ 不修改上游任何代码。本文件只请求裁定。
- ✅ 隐私红线已落成测试，与裁定无关但值得一提：`gateway_summary_never_leaks_device_detail_on_any_tier`（摘要不含序列号/型号/路径/SELinux 原串），以及错误文案与命令 stdout 两个旁路出口的两条守门测试。

---

## 7. 影响面与当前状态

| 阶段 | 内容 | 状态 |
|---|---|---|
| P0/P1/P2 | 设备探测、能力地板、Gate、审计、投影脱敏、证据采集（screencap / logcat / dumpsys / bugreportz，256 KiB 分片 + digest 校验） | **已交付**：636 测试全绿 + 真机（华为 BLA-AL00，system-shell 档）端到端验证 |
| P3 | 写档：setprop / settings / setenforce + 文件快照 LIFO 回滚 | **代码就绪、生产接线被 B1 阻塞**：`Gate::decide` 需要归属裁决，而裁决的来源与形状未裁定 |
| P4 | root 档（`su`） | 需 su 设备实测，与 B1/B2 无关 |

B1 不是"实现有 bug"，而是**生产接线的最后一块拼图缺一个上游定义**：谁、以什么协议、把归属裁决交给设备。

---

## 8. 评审后请回填

请在本文档末尾追加结论段（或另开 ADR），至少明确：
1. Q1 执行点限定（接受 / 否决 + 替代路径）；
2. Q2 `AttributionVerdict` 是否上升为上游契约类型；
3. Q3 无主节点写档语义与 admin 路径形状；
4. B2 选项（a/b/c/d）与能力命名规范（点分 / 冒号）；
5. 若接受 §2 的限定，请同步修订本地 ADR-AND-001 §7.1（移植侧负责回写，并同步 `2026-10-28-readiness-assessment.md` 缺口表）。

---

## 9. 补充证据（2026-11-02，真机安全边界 review 之后）

B2 的原始论据是「写 `/data/data/<pkg>` 与写 `/system` 不是一回事」。本轮在 BLA-AL00（user 版、无 root、uid=2000 shell）上做分类器的对抗扇探，发现**读侧**的论据比写侧更强，而且其中一条根本不是路径问题：

| 实测（同一台设备、同一个 uid、未提权） | 移植侧此前的判定 | 现在 |
|---|---|---|
| `content query --uri content://sms/` 返回真实短信正文 | ReadOnly → §4.4 免审批，日志记作「read」 | ForeignRead |
| `content query --uri content://contacts/people` 返回真实联系人姓名 | 同上 | ForeignRead |
| `head -c 24 /data/anr/dumptrace_*` 读出别的进程的栈（root 属、0666，在 `drwxrwx--x system` 的目录里） | 同上 | ForeignRead |
| `ls /sdcard/DCIM` 可列相册 | 同上 | ForeignRead |
| `dd if=/dev/block/mmcblk0`（整块分区经 `key=` 操作数） | 同上 | ForeignRead |

三点请上游在裁定 B2 时一并考虑：

1. **「读不需要同意」这个前提在 Android 上不成立。** ADR-AND-001 §4.4 免审批只读档的理由是「什么都没变」；但在一台装着用户短信、联系人和相册的手机上，*取值*本身就是隐私事件，与是否改变设备状态无关。移植侧已新增 `Effect::ForeignRead` 表达这一类，并临时借用 `android:fs:system` 授权——这正是 B2 选项 (a)「新增 Node/设备安全域表」要收编的第一个成员。**借用最重的文件档是保守方向**：代价是多要一次审批，不代价是把别人的数据库交出去。
2. **携带敏感内容的操作数不一定是路径。** 上表前三条里两条的敏感物在 **URI** 里（`content://sms`），任何按前缀写的规则都不可能命中。若上游建能力注册表，建议把「数据主体是谁」与「经由什么文件系统路径」分成两个轴，否则注册表会复制移植侧刚踩过的盲区。
3. **`fs:outside-app` 这个名字本身在误导。** 写 `/sdcard`（用户自己的文件）是该能力名字承诺过的动作，读 `/sdcard` 则不是同一条规则——移植侧因此把媒体树只放进**读**侧名单。这类「同一能力名覆盖不了读与写两种方向」的形状，是 B2 里「能力粒度」问题的具体实例。

移植侧现状：以上全部已落成回归测试（`a_content_uri_names_a_person_not_a_path`、`reading_another_uids_files_is_not_free` 等），并在真机 harness §14 里逐条对账。B2 未裁定前，`ForeignRead` 继续借用 `fs:system`；裁定后改名是一次机械替换，不影响判定逻辑。

## 10. 沉默即认可：每条的默认值与截止日期（2026-11-02 补）

§8 请上游"回填结论"，但这份请求 10-29 写成、11-02 才第一次进入上游版本控制，中间**没有任何人收到过它**——投递失败不是上游的疏忽，是提出方把"写进自己工作树"当成了"提交上游"。所以这一节把开放问题改写成可默认执行的选项：移植侧已经按其中一列在跑，答复只是确认或否决，不需要从零设计。

| 问题 | 候选 | 移植侧当前按哪个在跑（可复核） | 若否决，改动面 |
|---|---|---|---|
| Q1 执行点 | 接受 / 否决+替代路径 | **接受**。Gateway 判归属、设备判可行、设备做合取，已落地为契约层规则 R7，由 `tests/rules_conformance.rs` 的三个测试钉住 | 一处调用点 + R7 的测试期望 |
| Q2 `AttributionVerdict` 是否上升为上游契约类型 | 上升 / 暂不上升 | **暂不上升**。类型留在本仓库契约层，形状已按上游线格式对齐 | 上游若上升：本端口删自定义类型、改 import |
| Q3 无主节点写档语义 | fail-closed / admin 可 manage | **fail-closed**。无 owner 即写档不放行，与 D7 一致 | 若开 admin 路径：新增一条放行分支 + 审计字段 |
| B2 设备侧安全域表 | a / b / c / d | **无默认值**。本端口缺的不是实现而是这张表，当前以「借用 + 显式标注」过活：app-data 写借 `fs:system`，`content://` 读借 `fs:system`，网络出口**无处可借，所以直接拒**（`ping`/`nslookup`/`getent` 全档拒绝） | 选 (a) 则新增一张 Node/设备安全域表，本端口三处借用改为查表 |

**截止日期建议：2026-11-16（两周）。** 到期无答复即按「当前在跑」那一列继续，并在 `2026-10-28-readiness-assessment.md` 缺口表把对应行标为「按默认值执行，待追认」。

为什么只有这张表能这么办：Q1/Q2/Q3 三项都有可复核的落地物（规则、类型、门控分支），沉默的成本是继续跑现状，不会跑出一个未实现的东西；而 B2 那一行**没有默认值**，到期也不会被"默认"掉——它是真的需要有人写一张表。那一行按 §9 的证据单独升级，不混在本节的沉默机制里。

答复人：ADR-AND-001 §7.1 字面修订的作者（Q1/Q2/Q3），以及 ADR-087 D2/D7 的裁定者（Q3 的无主语义）。

## 11. 一条前提错误：本移植实现的 `ExecutionBackend` 在上游不存在（2026-11-02 核查）

移植侧的待办里记着"node 侧 `ReversibleBackend` 组装点"，隐含假设是上游有一个执行后端接口等着被实现。核查结果不支持这个假设：

- `ExecutionBackend` 定义在**本移植仓库自己的** `crates/acowork-platform-contract/src/exec.rs:245`；
- 上游 `core/` 全仓对 `ExecutionBackend` / `acowork-platform-contract` / `acowork-android` 的引用数为 **0**；
- 上游的执行抽象是 `core/acowork-core/src/tools/traits.rs:75` 的 `pub trait Tool: Send + Sync`，配套 `ToolInput`/`ToolOutput`；而 `core/acowork-runtime/src/tools/wasm/wasi_mapper.rs:11` 把 `shell` 明确列为 "No WASI mapping (host-controlled)"。

于是"接线"不是一个可以单方面完成的编码任务：要么上游采纳这份契约（§7.1 的执行点裁定才有落点），要么本移植实现上游的 `Tool`。这是第四个问题，它阻塞的不是某个函数调用，而是整个交付形态。

**默认值**：本端口继续按自己的契约跑——`acowork-host` 二进制已能在真机上端到端执行 probe→assemble→load→chain→exec→restore，不依赖任何上游接口即可验证。两种收敛方向的成本都只是一个 adapter，所以本条不抢跑。

**截止日期同 §10（2026-11-16）**；到期无答复即继续，并把这条并入 B4 的形态讨论一起回答——形态问题（APK 设置界面归谁）和接口问题（宿主后端归谁）其实是同一个问题的两面。

答复人：`Tool` 接口的维护方（acowork-core tools），以及 ADR-AND-001 §7.1 的作者。
