# acowork-gateway — Gateway

**定位**：常驻系统级进程，管理 Bot 生命周期、Intent 路由、密钥分发、预算协调。**不代理 Bot 业务逻辑**。

```
crates/acowork-gateway/
├── Cargo.toml
└── src/
    ├── main.rs                    # CLI 入口（系统托盘 / 守护进程）
    ├── lib.rs                     # 库入口
    ├── gateway/
    │   ├── mod.rs                 # Gateway 主循环 + 事件驱动
    │   └── state.rs               # 全局状态（已安装 Agent、运行中 Agent 等）
    ├── package_manager/
    │   ├── mod.rs
    │   ├── install.rs             # .bot 包安装（解压 + 签名验证 + manifest 校验）
    │   ├── uninstall.rs           # 卸载（可选备份 SQLite 记忆层）
    │   ├── upgrade.rs             # 升级（保留 data/ + config/，校验签名一致）
    │   └── repository.rs          # 远程仓库源（HTTP，Phase 5）
    ├── lifecycle/
    │   ├── mod.rs
    │   ├── manager.rs             # Agent 进程生命周期管理
    │   ├── process.rs             # 子进程 spawn/kill/health-check
    │   └── trigger.rs             # 触发器调度（按需/定时/cron）
    ├── intent/
    │   ├── mod.rs
    │   ├── router.rs              # Intent 路由（target 直达 / pattern 匹配）
    │   ├── capability.rs          # Capability Registry（安装时索引）
    │   └── queue.rs               # 异步 Intent 队列
    ├── budget/
    │   ├── mod.rs
    │   ├── tracker.rs             # 用量统计 + 超限信号
    │   └── config.rs              # 预算配置（per-bot 日/月限额）
    ├── rate/
    │   ├── mod.rs
    │   └── limiter.rs             # 速率令牌分配（per-provider RPM/TPM）
    ├── vault/
    │   ├── mod.rs                 # Key Vault 门面（委托 acowork-vault crate）
    │   └── distributor.rs         # Key 一次性分发（通过 IPC 传输）
    ├── sandbox/
    │   ├── mod.rs
    │   ├── config.rs              # 从 manifest 生成沙箱配置
    │   ├── linux.rs               # bubblewrap 隔离
    │   ├── windows.rs             # Job Object + 受限令牌
    │   └── macos.rs               # sandbox-exec
    ├── ipc/
    │   ├── mod.rs
    │   ├── server.rs              # Gateway Service API 服务端
    │   ├── transport.rs           # 传输层（Unix Socket / Named Pipe / Local TCP）
    │   └── session.rs             # 连接会话管理
    ├── system_agent/
    │   ├── mod.rs                 # 系统 Agent 特权管理
    │   └── identity_injector.rs   # 冷启动身份注入
    ├── config.rs                  # Gateway 配置
    └── cli.rs                     # CLI 子命令定义
```

## 关键模块说明

### `lifecycle/manager.rs` — 生命周期管理

```rust
pub struct LifecycleManager {
    processes: HashMap<String, AgentProcess>,  // agent_id → 运行中进程
    trigger_mgr: TriggerManager,
}

struct AgentProcess {
    child: Child,
    workspace: PathBuf,
    started_at: Instant,
    idle_since: Option<Instant>,
}

impl LifecycleManager {
    /// 启动 Agent：spawn 进程 + 注入身份 + 分发 Key
    async fn start_agent(&mut self, agent_id: &str) -> Result<()>;
    
    /// 杀死 Agent：直接杀进程（状态由 SQLite 持久化）
    async fn stop_agent(&mut self, agent_id: &str) -> Result<()>;
    
    /// 空闲超时检查
    async fn check_idle_timeout(&mut self) -> Vec<String>;
    
    /// 健康检查
    async fn health_check(&self, agent_id: &str) -> AgentHealth;
}
```

## Gateway 定位

- `acowork-core` — 共享类型
- `acowork-sign` — 签名验证
- `acowork-vault` — 密钥存储与分发
- `tokio`, `clap`, `serde_json`, `tracing`
- `cron` — 定时触发器

## Feature Flags

```toml
[features]
default = []
sandbox-bubblewrap = []            # Linux bubblewrap 沙箱
sandbox-landlock = ["dep:landlock"] # Linux landlock 沙箱
```
