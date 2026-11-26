# acowork-vault + acowork-sign

## acowork-vault — 密钥加密存储

**定位**：集中管理 LLM API Key，加密存储，一次性分发。

```
crates/acowork-vault/
├── Cargo.toml
└── src/
    ├── lib.rs
    ├── vault.rs                   # Vault 主结构（open/store/retrieve）
    ├── encryption.rs              # ChaCha20-Poly1305 AEAD 加解密
    ├── key_derivation.rs          # 用户密码 → 主密钥派生（Argon2id）
    └── error.rs
```

### 关键 API

```rust
pub struct Vault {
    vault_dir: PathBuf,
    master_key: Option<SecretString>,  // 解锁后驻留内存
}

impl Vault {
    /// 创建或打开 Vault
    pub fn open(vault_dir: &Path) -> Result<Self>;
    
    /// 用密码解锁（派生主密钥）
    pub fn unlock(&mut self, password: &str) -> Result<()>;
    
    /// 存储密钥（加密后写入文件）
    pub fn store(&self, key_name: &str, secret: &str) -> Result<()>;
    
    /// 检索密钥（解密后返回 SecretString，零拷贝）
    pub fn retrieve(&self, key_name: &str) -> Result<SecretString>;
    
    /// 列出所有密钥名称（不返回值）
    pub fn list(&self) -> Result<Vec<String>>;
}
```

### 加密设计

- `chacha20poly1305` — AEAD 加密
- `rand` — CSPRNG
- `secrecy` — SecretString 零拷贝封装
- `sha2`, `hmac` — 密钥派生

### 多 Key 管理（Phase 2 扩展）

当前 Phase 1 每个 provider 只支持一个 API Key。Phase 2 将扩展为多 Key 池，支持轮换和故障转移。

**Vault 存储结构扩展：**

```
~/.config/agent-gateway/vault/
├── openai/
│   ├── key_0.enc          # 索引 0 的 Key
│   ├── key_1.enc          # 索引 1 的 Key
│   └── meta.json          # Key 池元数据
├── anthropic/
│   ├── key_0.enc
│   └── meta.json
└── vault.key
```

**meta.json schema：**

```json
{
  "keys": [
    {
      "index": 0,
      "preview": "sk-...abc",
      "status": "active",
      "added_at": "2026-04-15T10:00:00Z",
      "last_used_at": "2026-04-15T15:30:00Z",
      "error_count": 0,
      "rate_limited_until": null
    },
    {
      "index": 1,
      "preview": "sk-...def",
      "status": "active",
      "added_at": "2026-04-15T12:00:00Z",
      "last_used_at": null,
      "error_count": 0,
      "rate_limited_until": null
    }
  ],
  "rotation_strategy": "round_robin",
  "failover_on_error": true
}
```

**轮换策略：**

| 策略 | 说明 | 适用场景 |
|------|------|---------|
| `round_robin` | 按 Key 索引顺序轮转，分散用量 | 多 Key 均衡负载 |
| `failover` | 优先使用 index 0，失败后切 index 1 | 主备模式 |
| `least_recent` | 选择最近最少使用的 Key | 避免单个 Key 频繁触发限流 |

**Key 健康检查：** Bot Runtime 上报用量时附带错误信息（如 429/401）。Gateway 更新 meta.json 中的 `error_count` 和 `rate_limited_until`。Key 分发时跳过 `status = "suspended"` 或仍在 rate limit 冷却期的 Key。

**Vault API 扩展：**

```rust
impl Vault {
    /// 获取下一个可用 Key（按轮换策略选择）
    pub fn acquire_key(&self, provider: &str) -> Result<SecretString>;

    /// 报告 Key 使用结果（成功/失败/限流）
    pub fn report_key_status(&self, provider: &str, key_index: usize, status: KeyStatus) -> Result<()>;

    /// 添加新 Key 到指定 provider 的池中
    pub fn add_key(&self, provider: &str, secret: &str) -> Result<usize>;

    /// 移除指定 Key
    pub fn remove_key(&self, provider: &str, key_index: usize) -> Result<()>;
}
```

---

## acowork-sign — .bot 包签名/验签

**定位**：独立的签名工具链，提供 `acowork-keygen`、`acowork-sign`、`acowork-verify` 三个命令。

```
crates/acowork-sign/
├── Cargo.toml
└── src/
    ├── lib.rs
    ├── signing_block.rs           # Signing Block 数据结构
    ├── keygen.rs                  # 密钥对生成（Ed25519）
    ├── sign.rs                    # 签名（插入 Signing Block 到 ZIP）
    ├── verify.rs                  # 验签（提取 Signing Block + 校验摘要）
    ├── certificate.rs             # X.509 证书处理
    └── error.rs
```

### 关键数据结构

```rust
pub struct SigningBlock {
    pub signers: Vec<Signer>,
}

pub struct Signer {
    pub certificates: Vec<Certificate>,     // X.509 证书链
    pub digest_algorithm: DigestAlgorithm,  // SHA-256
    pub digests: Vec<SectionDigest>,        // 各 section 摘要
    pub signature: Vec<u8>,                 // 对 digests 的签名
    pub signed_attrs: SignedAttributes,     // 签名时间戳等
}

pub enum SignerIdentity {
    Developer,           // 自签名
    Platform,            // 平台签名（系统 Agent）
    CaIssued,            // CA 签发（商店 Agent）
}
```

### 依赖

- `ed25519-dalek` — Ed25519 签名
- `x509-cert` — X.509 证书
- `sha2` — SHA-256 摘要
- `zip` — ZIP 操作
- `clap` — CLI
