# acowork-gateway — Gateway

**Position**: persistent system-level process, manages Bot lifecycle, Intent routing, key distribution, budget coordination. **Does not proxy Bot business logic**.

```
crates/acowork-gateway/
├── Cargo.toml
└── src/
    ├── main.rs                    # CLI entry (system tray / daemon)
    ├── lib.rs                     # library entry
    ├── gateway/
    │   ├── mod.rs                 # Gateway main loop + event-driven
    │   └── state.rs               # global state (installed Agents, running Agents etc.)
    ├── package_manager/
    │   ├── mod.rs
    │   ├── install.rs             # .agent package install (extract + signature verify + manifest check)
    │   ├── uninstall.rs           # uninstall (optional backup SQLite memory)
    │   ├── upgrade.rs             # upgrade (preserve data/ + config/, verify signature consistency)
    │   └── repository.rs          # remote repository source (HTTP, Phase 5)
    ├── lifecycle/
    │   ├── mod.rs
    │   ├── manager.rs             # Agent process lifecycle management
    │   ├── process.rs             # child process spawn/kill/health-check
    │   └── trigger.rs             # trigger scheduling (on-demand/scheduled/cron)
    ├── intent/
    │   ├── mod.rs
    │   ├── router.rs              # Intent routing (target direct / pattern match)
    │   ├── capability.rs          # Capability Registry (indexed at install)
    │   └── queue.rs               # async Intent queue
    ├── budget/
    │   ├── mod.rs
    │   ├── tracker.rs             # usage statistics + over-limit signal
    │   └── config.rs              # budget config (per-agent daily/monthly limit)
    ├── rate/
    │   ├── mod.rs
    │   └── limiter.rs             # rate token allocation (per-provider RPM/TPM)
    ├── vault/
    │   ├── mod.rs                 # Key Vault facade (delegates to acowork-vault crate)
    │   └── distributor.rs         # one-time key distribution (via IPC transfer)
    ├── sandbox/
    │   ├── mod.rs
    │   ├── config.rs              # generate sandbox config from manifest
    │   ├── linux.rs               # bubblewrap isolation
    │   ├── windows.rs             # Job Object + restricted token
    │   └── macos.rs               # sandbox-exec
    ├── ipc/
    │   ├── mod.rs
    │   ├── server.rs              # Gateway Service API server
    │   ├── transport.rs           # transport layer (Unix Socket / Named Pipe / Local TCP)
    │   └── session.rs             # connection session management
    ├── system_agent/
    │   ├── mod.rs                 # system Agent privilege management
    │   └── identity_injector.rs   # cold-start identity injection
    ├── config.rs                  # Gateway config
    └── cli.rs                     # CLI subcommand definitions
```

## Key Module Descriptions

### `lifecycle/manager.rs` — Lifecycle Management

```rust
pub struct LifecycleManager {
    processes: HashMap<String, AgentProcess>,  // agent_id → running process
    trigger_mgr: TriggerManager,
}

struct AgentProcess {
    child: Child,
    workspace: PathBuf,
    started_at: Instant,
    idle_since: Option<Instant>,
}

impl LifecycleManager {
    /// Start Agent: spawn process + inject identity + distribute Key
    async fn start_agent(&mut self, agent_id: &str) -> Result<()>;
    
    /// Kill Agent: directly kill process (state persisted via SqliteStore)
    async fn stop_agent(&mut self, agent_id: &str) -> Result<()>;
    
    /// Idle timeout check
    async fn check_idle_timeout(&mut self) -> Vec<String>;
    
    /// Health check
    async fn health_check(&self, agent_id: &str) -> AgentHealth;
}
```

## Gateway Dependencies

- `acowork-core` — shared types
- `acowork-sign` — signature verification
- `acowork-vault` — key storage and distribution
- `tokio`, `clap`, `serde_json`, `tracing`
- `cron` — scheduled triggers

## Feature Flags

```toml
[features]
default = []
sandbox-bubblewrap = []            # Linux bubblewrap sandbox
sandbox-landlock = ["dep:landlock"] # Linux landlock sandbox
```