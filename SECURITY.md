# Security Policy

ACowork is a decentralized AI agent runtime platform that executes agent packages,
manages user accounts, and stores credentials and conversations on the user's machine.
A vulnerability here can mean code execution, credential theft, or cross-user data
exposure. Please report it responsibly.

---

## Reporting a vulnerability

**Do not open a public issue for a security problem.** Do not open a public PR, and do
not post details in a discussion, issue, or chat — including in the repository's own
issue tracker.

Report privately via one of these channels:

| Channel | Use it for |
|---|---|
| **GitHub Security Advisory** (private) | Everything in this repository. Open one via **Security → Report a vulnerability** on the GitHub mirror (`github.com/tranxon/ACowork`). |
| **Gitee private issue** | If you do not have a GitHub account. The Gitee mirror is `gitee.com/tranxon/ACowork`. Mark it as private and contact the maintainer directly. |

Please include:

- **Type** — which component is affected (Gateway, Runtime, Node, user service, vault,
  signer, Desktop/Mobile app, WASM tool host, …).
- **What an attacker can do**, concretely. "Arbitrary file read in the Gateway" is a
  report; "the validation looks weak" is not.
- **Steps to reproduce**, including any config or `AUTH_MODE` the reproduction needs.
- **Impact** — what is reachable: another user's data, a stored credential, code
  execution as the local OS user.
- **Your environment** — OS, commit SHA, deployment topology (single machine / LAN /
  cloud relay).

### What to expect

| Stage | Target |
|---|---|
| Acknowledgement | 3 business days |
| Initial assessment (severity + whether we agree it is a bug) | 7 days |
| Fix or a published mitigation plan | 14 days for critical/high |

We will keep you updated while we work on it, and we will credit you in the advisory
unless you ask us not to.

### Reporting a vulnerability in a third-party dependency

Please report it to the upstream project first. If the ACowork release line pins a
vulnerable version and we cannot mitigate it, that is a legitimate report against this
repository — tell us the upstream issue and the pinned version.

---

## Security model — what ACowork does and does not guarantee

Understanding this section is what makes a report useful, and what stops us from
chasing reports that are not bugs.

### The trust boundary: the local OS user

ACowork runs as a desktop application under a single OS user account. The **physical OS
user is the root of trust** for the local deployment. Anything the OS user can read, the
Gateway can read.

This is why the two auth modes exist:

| Auth mode | Binding | Trust boundary | Account system |
|---|---|---|---|
| `local` | loopback only | The OS user. Legacy bearer-token auth, no login, no admin role, session filtering disabled. | ADR-076 §1–§11 are no-ops. |
| `multi_user` | non-loopback | The account system + the OS user | Full ADR-076 account system: login tokens, admin role, session isolation, user-to-user chat. |

`AUTH_MODE` is resolved by explicit CLI flag → explicit TOML → bind-address inference →
default `local`. Note the consequence: **binding to `0.0.0.0` implies `multi_user`**.
If you expose a Gateway to the LAN or the internet, the full account system is active —
but so is the full attack surface.

### Process architecture

```
Desktop App (Tauri v2)          separate process, presentation only
  |
Gateway                         HTTP :19876 (Axum) + embedded MQTT broker :19875
  |                            reverse proxy, lifecycle, package management
  |
Node Agent                     per-machine daemon, process table, reverse proxy :19900
  |
Agent Runtime                  MQTT client + localhost HTTP, hosts agent processes
```

Each Agent is a **declarative `.agent` package** — a manifest, prompts, and skills. It
contains **no executable code**. The Runtime is a trusted binary; the agent package is
data.

### What is protected

| Asset | How |
|---|---|
| User account passwords | Argon2id (m=64 MiB, t=3, p=4), stored as a self-describing PHC string. Verification is constant-time. |
| Stored API keys / secrets | `acowork-vault`: a password-derived master key (Argon2id) encrypts values with ChaCha20-Poly1305. The master key is never persisted in plaintext. |
| Agent packages | Ed25519 signature over the **entire ZIP content** (local files + central directory + end of directory) — there is no unsigned region. Unsigned or invalidly signed packages are rejected at install. |
| Agent upgrades | The new package's certificate fingerprint must match the installed version. |
| User chat sessions | Isolated per user (`user_id`), with server-side single-point enforcement. |
| Cross-user requests | Three-tier ownership model (ADR-087): `view` / `use` / `manage`, enforced at the Gateway's reverse-proxy entry, **fail-closed** — an unregistered route defaults to `manage`. |

### What is explicitly *not* guaranteed

This is the part that matters when triaging a report:

- **ACowork does not sandbox the agent's own reasoning.** An agent's prompts and skills
  are executed by the LLM. Prompt injection against an agent is a **model** problem, not
  a cryptographic one. The mitigations are the permission model (an agent can only do
  what it declares and the user approved) and the Approval Gate for risky operations.
- **The OS-user boundary is not enforced against the agent's tools.** WASM custom tools
  run in a Wasmtime sandbox and cannot read host memory. Shell access, where granted, is
  a deliberate, declared, user-approved grant — it runs with the OS user's own
  authority. See [Shell safety](#shell-safety-is-a-declared-grant-not-a-sandbox).
- **ACowork does not verify the identity of a package publisher.** A valid signature
  proves the package was signed by the holder of a particular private key. It does not
  prove who that person is, and it does not prove the agent behaves well.
- **The `local` auth mode has no user isolation by design.** Do not report "user A can
  see user B's data in `local` mode" — that is the documented trust boundary, not a
  vulnerability. It *is* a vulnerability if `multi_user` mode leaks across users.
- **Availability of a self-hosted deployment is the operator's responsibility.** We do
  not ship a DDoS-resistant public service.

---

## Shell safety is a declared grant, not a sandbox

Shell execution is the highest-risk capability in the system, and it is governed by the
permission model rather than by a kernel sandbox.

- An agent must **declare** every permission it needs. The runtime enforces the
  declaration, and the user can refuse a permission at install time.
- Network access is **deny by default** and must be declared per domain
  (e.g. `network:https://api.openai.com`).
- Shell commands are **risk-graded**, and high-risk operations route through the
  Approval Gate for user confirmation.
- The audit log records what ran, under whose identity.

If you find a path where an agent reaches a capability it did **not** declare, or
reaches one the user **refused**, that is a reportable vulnerability — regardless of
whether the underlying operation was "only" a file read.

---

## Hardening guidance for operators

If you run ACowork for yourself or for a team:

- **Keep the bind address on loopback** unless you are deliberately running a multi-user
  deployment. `local` is the default and the smaller surface.
- **When exposing it**, set `AUTH_MODE=multi_user` explicitly and understand that the
  account system is the only thing standing between a network peer and another user's
  sessions.
- **Give the Gateway a dedicated OS account** on shared machines, so a compromise of the
  process does not also compromise the user's other files.
- **Treat the Vault as the crown jewel.** It is unlocked by the user's password. A
  weak password weakens every stored key.
- **Verify package signatures** before distributing `.agent` packages. Do not disable
  signature verification for convenience.
- **Keep the relay tier in mind.** The cloud relay is a thin BYO relay, not a
  trusted component — it forwards traffic; it is not a place to put secrets.

---

## Hardening guidance for contributors

- **Never commit a private key, a credential, or a `.env` file.** Check
  `git diff --cached` before committing. If you have committed a private key, **rotate
  it** — removing the file in a later commit does not remove it from history.
- **Do not weaken a red line to make a test pass.** The red lines in `dev/ci.sh` encode
  bug classes that have shipped. If you believe a boundary is wrong, that is an ADR,
  not a lint suppression.
- **Do not relax the permission model.** A new route must be classified in
  `core/acowork-gateway/src/http/permission.rs`. If you add a route that is not
  registered, it falls through to the `manage` tier — register it properly rather than
  adding a bypass.
- **Do not log secrets.** Not API keys, not tokens, not password material — not even
  at `debug` level.
- **Do not add a `println!` that prints a config, a header, or an `AuthContext`.**
- **Agent packages must not contain executable code, obfuscation, or binary blobs**, and
  must not fetch and execute remote code at runtime. Declare a tool instead.
- **Keep `acowork-eval` out of shipping binaries.** It is a dev-dependency by design.

---

## Security-relevant architecture references

| Area | Document |
|---|---|
| Full security design | [`docs/design/zh/08-security.md`](docs/design/zh/08-security.md) |
| Gateway / Runtime filesystem isolation | [ADR-009](docs/adr/zh/ADR-009-gateway-workspace-isolation.md) |
| Multi-user account system | [ADR-076](docs/adr/zh/ADR-076-multi-user-account-system.md) |
| User domain as a standalone process | [ADR-084](docs/adr/zh/ADR-084-user-standalone-process.md) |
| Node / Agent owner permission model | [ADR-087](docs/adr/zh/ADR-087-node-agent-owner-permissions.md) |
| Remote runtime / node topology | [ADR-055](docs/adr/zh/ADR-055-remote-runtime-node-topology.md) |
| Agent package format & signing | [`docs/design/zh/02-agent-package.md`](docs/design/zh/02-agent-package.md) |

---

## Reporting a security issue in an agent package

If you publish `.agent` packages separately, the same rules apply: report privately,
do not disclose the exploit details publicly, and give the maintainers time to ship a
fix or withdraw the package before disclosing.

If **your** package has a vulnerability, please disclose it to your own users directly
and let the ACowork maintainers know what the fix looks like, so a patched version can
be signed and distributed.

---

## Code of Conduct

Security reports are handled by the same maintainers, under the same expectations of
professional conduct as every other interaction. See
[`CODE_OF_CONDUCT.md`](CODE_OF_CONDUCT.md).
