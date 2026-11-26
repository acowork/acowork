# acowork-vault + acowork-sign

## acowork-vault — Encrypted Key Storage

**Position**: Centralized management of LLM API Keys, encrypted storage, one-time distribution.

```
crates/acowork-vault/
├── Cargo.toml
└── src/
    ├── lib.rs
    ├── vault.rs                   # Vault main structure (open/store/retrieve)
    ├── encryption.rs              # ChaCha20-Poly1305 AEAD encryption/decryption
    ├── key_derivation.rs          # user password → master key derivation (Argon2id)
    └── error.rs
```

### Key API

```rust
pub struct Vault {
    vault_dir: PathBuf,
    master_key: Option<SecretString>,  // resides in memory after unlock
}

impl Vault {
    /// Create or open Vault
    pub fn open(vault_dir: &Path) -> Result<Self>;
    
    /// Unlock with password (derive master key)
    pub fn unlock(&mut self, password: &str) -> Result<()>;
    
    /// Store key (write encrypted to file)
    pub fn store(&self, key_name: &str, secret: &str) -> Result<()>;
    
    /// Retrieve key (return SecretString after decryption, zero-copy)
    pub fn retrieve(&self, key_name: &str) -> Result<SecretString>;
    
    /// List all key names (don't return values)
    pub fn list(&self) -> Result<Vec<String>>;
}
```

### Encryption Design

- `chacha20poly1305` — AEAD encryption
- `rand` — CSPRNG
- `secrecy` — SecretString zero-copy wrapping
- `sha2`, `hmac` — key derivation

### Multi-Key Management (Phase 2 Extension)

Currently Phase 1 supports only one API Key per provider. Phase 2 will extend to multi-key pool, supporting rotation and failover.

**Vault storage structure extension:**

```
~/.config/agent-gateway/vault/
├── openai/
│   ├── key_0.enc          # Key at index 0
│   ├── key_1.enc          # Key at index 1
│   └── meta.json          # Key pool metadata
├── anthropic/
│   ├── key_0.enc
│   └── meta.json
└── vault.key
```

**meta.json schema:**

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

**Rotation strategies:**

| Strategy | Description | Use Case |
|----------|-------------|----------|
| `round_robin` | Rotate by Key index order, distribute usage | Multi-key load balancing |
| `failover` | Prefer index 0, switch to index 1 on failure | Active-standby mode |
| `least_recent` | Select least recently used Key | Avoid frequent rate limiting on single Key |

**Key health check**: Bot Runtime reports usage with error info (e.g. 429/401). Gateway updates `error_count` and `rate_limited_until` in meta.json. Key distribution skips Keys with `status = "suspended"` or still in rate limit cooldown.

**Vault API extensions:**

```rust
impl Vault {
    /// Get next available Key (select per rotation strategy)
    pub fn acquire_key(&self, provider: &str) -> Result<SecretString>;

    /// Report Key usage result (success/failure/rate-limited)
    pub fn report_key_status(&self, provider: &str, key_index: usize, status: KeyStatus) -> Result<()>;

    /// Add new Key to specified provider's pool
    pub fn add_key(&self, provider: &str, secret: &str) -> Result<usize>;

    /// Remove specified Key
    pub fn remove_key(&self, provider: &str, key_index: usize) -> Result<()>;
}
```

---

## acowork-sign — .bot Package Sign/Verify

**Position**: Independent signing toolchain, provides `acowork-keygen`, `acowork-sign`, `acowork-verify` three commands.

```
crates/acowork-sign/
├── Cargo.toml
└── src/
    ├── lib.rs
    ├── signing_block.rs           # Signing Block data structures
    ├── keygen.rs                  # key pair generation (Ed25519)
    ├── sign.rs                    # sign (insert Signing Block into ZIP)
    ├── verify.rs                  # verify (extract Signing Block + check digest)
    ├── certificate.rs             # X.509 certificate handling
    └── error.rs
```

### Key Data Structures

```rust
pub struct SigningBlock {
    pub signers: Vec<Signer>,
}

pub struct Signer {
    pub certificates: Vec<Certificate>,     // X.509 certificate chain
    pub digest_algorithm: DigestAlgorithm,  // SHA-256
    pub digests: Vec<SectionDigest>,        // per-section digests
    pub signature: Vec<u8>,                 // signature over digests
    pub signed_attrs: SignedAttributes,     // signature timestamp etc.
}

pub enum SignerIdentity {
    Developer,           // self-signed
    Platform,            // platform-signed (system Agent)
    CaIssued,            // CA-issued (store Agent)
}
```

### Dependencies

- `ed25519-dalek` — Ed25519 signing
- `x509-cert` — X.509 certificates
- `sha2` — SHA-256 digest
- `zip` — ZIP operations
- `clap` — CLI