# acowork-relay 境内自建部署 — 阿里云 ECS + 境内域名 + ICP 备案

> 目标：在**中国大陆**的 ECS 上自建 `acowork-relay`，供**自用**（自己或自己的团队访问自己的 Gateway）。
> 依据：[设计文档 24](../design/zh/24-cloud-relay-remote-access.md)（SNI 路由与中继职责边界）、
> [ADR-088](../adr/zh/ADR-088-cloud-hub-application-layer-remote-access.md)（Relay 定位为自托管 BYO 档，协议冻结在 `proto=1`）。
> 姊妹篇：[relay-ecs-cloudflare-deploy.md](./relay-ecs-cloudflare-deploy.md)（Cloudflare 注册 + 泛解析路径）。
>
> **本文按顺序执行，每步自带验证命令。验证不通过就别往下走** —— 后面的步骤都建立在前一步已通过的前提上。

---

## 0. 先读这一节

### 0.1 适用前提（不满足就别用本文）

| 项 | 要求 |
|---|---|
| ECS 地域 | **中国大陆**。境外节点见 §13，那条路在境内网络下**不可用** |
| 域名 | **可在境内完成 ICP 备案**：注册商是工信部批复机构、TLD 在批复名单内、已完成实名认证 |
| 使用性质 | **自用**（不向他人提供接入 / 组网服务） |
| DNS | 能添加 A 记录与**泛解析**（`*`） |

### 0.2 三条硬约束

违反任何一条，中继都不工作：

| # | 约束 | 原因 |
|---|------|------|
| 1 | **必须有域名，不能裸 IP** | 中继按 `<gw-id>.<device_domain_suffix>` 路由（`gw_id_from_sni`，见 [config.rs:68](../../core/acowork-relay/src/config.rs#L68) 与 [entry.rs:330](../../core/acowork-relay/src/entry.rs#L330)）。裸 IP **没有前缀空间** → 设备域永远匹配不上，连接被直接关闭。裸 IP 只能开控制面（`/health`），开不了设备域 |
| 2 | **`relay.<域名>` 与 `*.<域名>` 都必须解析到 ECS 公网 IP** | 前者是控制面（`/health`、`/tunnel`），后者承载每个 Gateway 的设备域 |
| 3 | **安全组放行 443/TCP 入站** | 中继自己监听 443（以 `CAP_NET_BIND_SERVICE` 非 root 绑定） |

> `service_domain` 与 `device_domain_suffix` **填同一个值**是合法的：SNI 精确匹配优先于后缀匹配（[main.rs:62](../../core/acowork-relay/src/main.rs#L62)）。

### 0.3 关于「牌照」与「备案」——两件不同的事

- **自用自建不构成经营电信业务，不需要增值电信业务经营许可证（牌照）。**
- 需要的是 **ICP 备案**：域名指向境内 ECS 提供 80/443 服务时的前置条件。

> ⚠️ **备案 ≠ 牌照。** 本文只涉及备案。但注意：备案**是**必需的，不是可选项 —— 见下一节。

### 0.4 「跑几天后突然不通」的根因

**境内 ECS + 未备案域名**是最常见的原因。云厂商对未备案域名的拦截由**周期性扫描**触发，所以表现为「先能用、过几天挂掉」，而不是一开始就失败。§1 的备案是根治手段。

另一个同症状的原因是**实例侧**（欠费 / 试用到期 / 带宽超限 / 突发性能实例 CPU 积分耗尽 / 非 EIP 公网 IP 变更）—— 见 §11。

---

## 1. ICP 备案

### 1.1 先检查域名能不能备案

| 检查项 | 通过条件 | 不通过怎么办 |
|---|---|---|
| 注册商 | 工信部批复的域名注册服务机构（阿里云 / 腾讯云 / 万网等） | **境外注册商无法备案**（Cloudflare 注册的域名即属此类）→ 转入境内注册商 |
| TLD | 在工信部批复名单内（`.com` / `.cn` / `.net` 等） | 换一个批复名单内的 TLD |
| 实名认证 | 域名已完成实名认证，且与备案主体一致 | 先在注册商处完成实名 |

### 1.2 申请备案服务码

阿里云控制台 → **备案** → 用**这台 ECS** 申请备案服务码（每台 ECS 可申请多个）。

```bash
# 记下 ECS 实例 ID，申请服务码时要用
# 控制台 → 云服务器 ECS → 实例 → 实例 ID
```

### 1.3 提交备案

1. **主体**：建议用**企业主体**。个人备案对「提供网络服务」类用途容易被驳回。
2. 填写域名、服务器（ECS 实例）、负责人信息。
3. 上传证件、完成核验（阿里云 App 人脸核验）。

**时间预期**：阿里云初审约 1–2 个工作日 → 管局审核一般 **3–20 个工作日**。期间域名不可用于对外服务。

### 1.4 验证备案通过

```bash
# 工信部备案系统查询（浏览器打开）
# https://beian.miit.gov.cn  → 查询你的域名，应显示备案号
```

### 1.5 公安联网备案（备案通过后）

备案通过后，通常需要在 **30 日内**到「全国互联网安全管理服务平台」完成公安联网备案。**是否适用请按当地要求核实**，这条与本中继本身的功能无关，但漏掉会被要求整改。

---

## 2. DNS 配置

在域名的 DNS 服务商处添加两条 A 记录：

| Type | Name | Content | 说明 |
|------|------|---------|------|
| `A` | `relay` | `<ECS 公网 IP>` | 控制面 |
| `A` | `*` | `<ECS 公网 IP>` | **泛解析**，覆盖所有 `<gw-id>.relay.<域名>` |

> ⚠️ **若 DNS 托管在 Cloudflare：两条记录都必须是「仅 DNS / 灰云」。** 橙云代理会在 Cloudflare 边缘终结 TLS，只回明文 HTTP，而中继靠 **TLS SNI** 判断是控制面还是设备隧道 —— SNI 到不了源站就无法路由。（与姊妹篇同一条约束）

**验证**（等 DNS 生效后执行）：

```bash
dig +short relay.<你的域名>          # 应返回 ECS 公网 IP
dig +short x.relay.<你的域名>        # 应返回同一个 IP（验证泛解析）
```

若 DNS 在 Cloudflare，两个都必须**直接是 ECS IP**，不能是 Cloudflare 边缘 IP（`104.x` / `172.64.x` 等）。

---

## 3. ECS 安全组

控制台 → ECS → 实例 → 安全组 → 入方向规则：

| 方向 | 协议 | 端口 | 授权对象 | 用途 |
|------|------|------|---------|------|
| 入方向 | TCP | `443/443` | `0.0.0.0/0` | relay TLS 监听 |
| 入方向 | TCP | `22/22` | **你的办公 IP/32** | SSH，不要对全网开放 |
| 出方向 | TCP | `80` + `443` | `0.0.0.0/0` | 依赖拉取、证书续期 |

**验证**（在 ECS 上）：

```bash
sudo ss -tlnp | grep :443 || echo "443 空闲，服务还没起（正常）"
```

---

## 4. 系统初始化

以 **root 或 sudoer 账号**登录 ECS（`acowork-relay` 是后面给 systemd 用的 nologin 系统账号，不要用它登录）。

```bash
sudo useradd -r -s /usr/sbin/nologin acowork-relay
sudo install -d -o acowork-relay -g acowork-relay -m 0750 /var/lib/acowork-relay
sudo install -d -m 0750 /etc/acowork-relay
```

**验证**：

```bash
id acowork-relay        # 应显示 uid，且 shell 是 /usr/sbin/nologin
```

---

## 5. 签发通配符证书（DNS-01）

泛域名**只有 DNS-01 能签**（HTTP-01 签不了通配符）。按 DNS 在哪选路线：

### 路线 A：DNS 在阿里云（推荐，与本文其余部分配对）

官方 certbot **没有**阿里云 DNS 插件，用 `acme.sh`（内置 `dns_ali`）：

```bash
# 安装 acme.sh（会自动装 cron 续期任务）
curl https://get.acme.sh | sh -s email=<你的邮箱>

# 阿里云 AccessKey（需要 AliyunDNSFullAccess 权限；建议单独建一个 RAM 子账号）
export Ali_Key="<AccessKeyId>"
export Ali_Secret="<AccessKeySecret>"

# 签发（DNS-01）
~/.acme.sh/acme.sh --issue --dns dns_ali \
  -d relay.<你的域名> -d '*.<你的域名>'
```

### 路线 B：DNS 仍在 Cloudflare

沿用姊妹篇的 certbot 路径：

```bash
sudo certbot certonly --dns-cloudflare \
  --dns-cloudflare-credentials /etc/letsencrypt/api-credentials/cloudflare.ini \
  -d relay.<你的域名> -d '*.<你的域名>'
```

> 注意：**备案看的是注册商和 TLD，不是 DNS 托管地**。注册商合规的域名，DNS 放在 Cloudflare 也可以（Cloudflare 必须灰云，见 §2）。

### 5.1 把证书安装到 unit 期望的路径

systemd unit 模板里的证书路径是 `/etc/letsencrypt/live/relay.example.com/`，且 §8 会用 `sed` 批量替换域名 —— 所以把证书**装到这个命名约定下**，就能让 unit 模板原样工作。

```bash
DOMAIN=relay.<你的域名>
sudo install -d -m 0755 /etc/letsencrypt/live/$DOMAIN

# 路线 A（acme.sh）
sudo ~/.acme.sh/acme.sh --install-cert -d $DOMAIN \
  --key-file       /etc/letsencrypt/live/$DOMAIN/privkey.pem \
  --fullchain-file /etc/letsencrypt/live/$DOMAIN/fullchain.pem \
  --reloadcmd      "/etc/acowork-relay/reload-cert.sh"
```

**验证**：

```bash
sudo ls -l /etc/letsencrypt/live/$DOMAIN/
# 期望：privkey.pem + fullchain.pem 两个文件

echo | openssl x509 -in /etc/letsencrypt/live/$DOMAIN/fullchain.pem -noout -text \
  | grep -A1 "Alternative Name"
# 期望：DNS:relay.<你的域名>, DNS:*.<你的域名>   ← 两个都要有
```

---

## 6. 证书可读权限（relay 以非 root 运行）

unit 里 relay 以 `User=acowork-relay` 运行，**读不到私钥服务就起不来**。

```bash
DOMAIN=relay.<你的域名>

sudo chown -R acowork-relay:acowork-relay /etc/letsencrypt/live/$DOMAIN
sudo chmod 750 /etc/letsencrypt/live/$DOMAIN
sudo chmod 640 /etc/letsencrypt/live/$DOMAIN/privkey.pem
sudo chmod 644 /etc/letsencrypt/live/$DOMAIN/fullchain.pem

# 父目录必须可穿越
sudo chmod 755 /etc/letsencrypt /etc/letsencrypt/live
```

**验证**（这一条不过，服务一定起不来）：

```bash
DOMAIN=relay.<你的域名>
sudo -u acowork-relay head -c 1 /etc/letsencrypt/live/$DOMAIN/privkey.pem   && echo "← privkey OK"
sudo -u acowork-relay head -c 1 /etc/letsencrypt/live/$DOMAIN/fullchain.pem && echo "← fullchain OK"
```

诊断用（逐级显示路径权限，挡路的那一级一眼可见）：

```bash
namei -l /etc/letsencrypt/live/<你的域名>/privkey.pem
```

---

## 7. 编译 acowork-relay

在**本地**编译（Windows 用 WSL2、macOS 或 Linux 都行），不要在 ECS 上装 Rust 工具链。

先确认 ECS 架构（阿里云神龙 / 倚天实例是 **arm64**）：

```bash
# 在 ECS 上跑
uname -m          # x86_64 或 aarch64
```

| ECS 架构 | 编译 target |
|---|---|
| `x86_64` | `x86_64-unknown-linux-musl` |
| `aarch64` | `aarch64-unknown-linux-musl` |

**本地环境准备**：

```bash
# 依赖：C 工具链（ring 需要）+ musl
sudo apt install -y build-essential musl-tools        # WSL2/Linux
# brew install musl                                    # macOS

# rustup（国内设镜像，否则 static.rust-lang.org 卡住）
export RUSTUP_DIST_SERVER="https://rsproxy.cn"
export RUSTUP_UPDATE_ROOT="https://rsproxy.cn/rustup"
curl --proto '=https' --tlsv1.2 -sSf https://rsproxy.cn/rustup-init.sh | sh -s -- -y --profile minimal
source ~/.cargo/env
rustup target add <上面那个 target>

# cargo 也要换源
mkdir -p ~/.cargo && printf '[source.crates-io]\nreplace-with = "rsproxy-sparse"\n\n[source.rsproxy-sparse]\nregistry = "sparse+https://rsproxy.cn/index/"\n\n[net]\ngit-fetch-with-cli = true\n' > ~/.cargo/config.toml
```

**编译**：

```bash
cd ~/acowork
cargo build --release -p acowork-relay --target x86_64-unknown-linux-musl --manifest-path core/Cargo.toml
```

**验证**：

```bash
file core/target/x86_64-unknown-linux-musl/release/acowork-relay
# 期望：ELF 64-bit LSB executable, x86-64, statically linked, ...
```

必须显示 `statically linked`。若显示 `dynamically linked`，说明 musl 没生效。

**上传**（二进制 **和** unit 都要传）：

```bash
scp core/target/x86_64-unknown-linux-musl/release/acowork-relay root@<ECS公网IP>:/tmp/
scp dev/deploy/relay/acowork-relay.service root@<ECS公网IP>:/tmp/
```

---

## 8. 部署 systemd 服务

```bash
# 二进制 + unit 就位
sudo install -m 0755 /tmp/acowork-relay /usr/local/bin/acowork-relay
sudo cp /tmp/acowork-relay.service /etc/systemd/system/
sudo install -d -o acowork-relay -g acowork-relay -m 0750 /var/lib/acowork-relay

# 改域名（模板里是 relay.example.com，4 处）
sudo sed -i 's/relay\.example\.com/relay.<你的域名>/g' /etc/systemd/system/acowork-relay.service
```

**验证替换到位**（4 处域名 + 2 个证书文件路径）：

```bash
sudo grep -E 'service-domain|device-domain-suffix|tls-cert|tls-key' /etc/systemd/system/acowork-relay.service
```

> unit 已设置 `ProtectSystem=strict` + `ReadWritePaths=/var/lib/acowork-relay`，证书目录只读即可，无需额外放行。

**启动**：

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now acowork-relay
sleep 2
sudo systemctl is-active acowork-relay     # 期望：active
```

若不是 `active`：

```bash
sudo systemctl stop acowork-relay          # 先停，否则 Restart=always 会无限刷屏
sudo journalctl -u acowork-relay -n 50 --no-pager
```

| 日志关键字 | 原因 |
|---|---|
| `Permission denied (os error 13)` | 证书权限没给全 → 回 §6 |
| `Address already in use` | 443 被占 → `sudo ss -tlnp \| grep :443` |
| `No such file or directory` + 域名 | `sed` 没替换成功 → 重跑上面的 `grep` |
| `tls_cert and tls_key must be provided together` | unit 里只替换了一个路径 → 检查 §8 的 `grep` 输出 |
| `depends on a target or service not named` | `/var/lib/acowork-relay` 不存在或属主不对 → 重跑 `install -d` |

---

## 9. 端到端验证

### 9.1 relay 对外可用

```bash
curl -sS https://relay.<你的域名>/health          # 期望：ok
```

```bash
echo | openssl s_client -connect relay.<你的域名>:443 -servername relay.<你的域名> 2>/dev/null \
  | openssl x509 -noout -text | grep -A1 "Alternative Name"
# 期望：DNS:relay.<你的域名>, DNS:*.<你的域名>
```

### 9.2 设备域路由（裸 IP 做不到的那一步）

泛解析必须先验证，否则 Desktop 连不上：

```bash
curl -s -o /dev/null -w '%{http_code}\n' https://deadbeef.relay.<你的域名>/health
# 期望：502（DEVICE_OFFLINE）—— 说明泛解析已通、SNI 路由已生效、只是该 gw-id 无隧道
# 若返回连接失败 / TLS 错误 → 泛解析或证书 SAN 有问题
```

> 502 是**预期结果**，不是故障：它证明「SNI 认出了设备域，但该 gw-id 没有活动隧道」。

### 9.3 Gateway 侧开启隧道

在你的 Gateway 机器上，`~/.acowork/acowork-gateway/config/gateway.toml`：

```toml
[relay]
enabled = true
url = "wss://relay.<你的域名>/tunnel"
```

或运行时启用：

```bash
curl -X POST http://127.0.0.1:19876/api/relay/enable \
  -H 'Content-Type: application/json' \
  -d '{"url":"wss://relay.<你的域名>/tunnel"}'
```

**验证**：

```bash
curl -s http://127.0.0.1:19876/api/relay/status | python3 -m json.tool
# 期望：{"enabled":true, "connected":true, "gw_id":"<UUID v4>"}
```

首次连接时 Gateway 会在 `~/.acowork/acowork-gateway/config/relay_identity.json` 生成 Ed25519 设备身份（私钥**永不上传**）。

### 9.4 Desktop 远程连接

1. Desktop 设置 → 连接模式选 **Relay**
2. Gateway URL 填 `https://<gw-id>.relay.<你的域名>`（`<gw-id>` 来自 9.3 的 `/api/relay/status`）
3. 用普通账号登录

> Debug 端点远程返回 404 属**预期**（远程 ACL 加固）：relay 模式自动禁用 DevMode、LSP Relay 显示不可用（设计文档 24 §8.2）。

---

## 10. 证书续期

中继**不热加载**证书，续期后必须重启才生效。

### 10.1 续期后自动重装 + 重启

`/etc/acowork-relay/reload-cert.sh`（§5.1 的 `--reloadcmd` 指向它）：

```bash
sudo tee /etc/acowork-relay/reload-cert.sh >/dev/null <<'SH'
#!/bin/sh
set -e
D=/etc/letsencrypt/live/relay.<你的域名>
chown -R acowork-relay:acowork-relay "$D"
chmod 750 "$D"
chmod 640 "$D/privkey.pem"
chmod 644 "$D/fullchain.pem"
systemctl restart acowork-relay
logger -t acowork-relay-cert "cert reinstalled, relay restarted"
SH
sudo chmod 0750 /etc/acowork-relay/reload-cert.sh
```

### 10.2 验证

```bash
# 手动跑一次续期流程（acme.sh 会自动判断是否需要续期）
sudo ~/.acme.sh/acme.sh --cron

# 确认续期定时任务已装
crontab -l | grep acme.sh        # 应有 acme.sh 一行

# 确认 hook 真的执行了
journalctl -t acowork-relay-cert --since "-5 min" --no-pager
```

> 路线 B（certbot）的续期 hook 见姊妹篇 §10.2 —— 注意 certbot 的 `--dry-run` **不执行** deploy hook，必须单独验。

---

## 11. 故障排查

### 11.1 「跑几天后突然不通」专用清单

按顺序查，**先排除实例侧，再怀疑网络侧**：

| # | 检查 | 命令 / 位置 | 命中后的结论 |
|---|---|---|---|
| 1 | **阿里云站内信 / 短信 / 邮件** | 控制台 → 消息中心 | 有违规/备案告知 → 就是它，按告知整改 |
| 2 | **实例状态** | 控制台 → ECS 实例 | `已停止` → 欠费 / 到期 |
| 3 | **余额与到期时间** | 控制台 → 费用中心 | 过期 → 续费 |
| 4 | **带宽计费方式与用量** | 控制台 → 实例 → 带宽 | 按量付费超出 / 流量包耗尽 → 限速或停机 |
| 5 | **CPU 积分**（突发性能实例 t5/t6） | 控制台 → 实例 → 监控 | 积分耗尽 → CPU 被限到基准，服务看起来"死"了 |
| 6 | **公网 IP 是否变了** | 对比控制台公网 IP 与 `dig` 结果 | 非 EIP 的公网 IP 在停/开机后**会变** → 同步 DNS |
| 7 | **服务与日志** | `systemctl status acowork-relay`、`journalctl -u acowork-relay -n 100` | 进程崩溃 / 证书读不到 |
| 8 | **隧道而非 relay** | `curl /health` 通 + `curl /api/relay/status` 异常 | 是 Gateway 隧道断了，不是 relay 挂了 |

**诊断用临时换端口**（仅诊断，**不要当生产方案**）：

```bash
# 临时监听一个非 443 端口，测 TCP 是否可达
# 能通 → 443 入方向被拦（备案 / 安全组 / 云侧策略）
# 不通 → 更可能是实例侧（§11.1 第 2–6 项）
```

### 11.2 通用排查表

| 症状 | 原因 | 处理 |
|------|------|------|
| `curl /health` 超时 | DNS 没指对 / 安全组 443 没开 / 服务没起 / 未备案被拦 | `dig +short relay.<域名>` 必须返回 ECS IP；`systemctl status`；查 §11.1 第 1 项 |
| `/health` 通但设备域 502 | Gateway 隧道未建立 | Gateway 日志 + `curl /api/relay/status` |
| 设备域报**连接失败 / TLS 错误**（不是 502） | 泛解析没配 / 证书缺通配符 SAN | 回 §2 与 §5.1 的两条验证 |
| Desktop TLS 报错 | 证书 SAN 不含 `*.<域名>` | 重跑 §5.1 验证 |
| 证书 60 天后失效 | 续期 hook 断了 | §10 验证；**不要**用 `--force-renewal` 硬试（LE 对相同域名集合有限流） |
| 裸 IP 只能开 `/health`，设备域永远连不上 | 设计如此：设备域需要 `<gw-id>.` 前缀，IP 无法构成 | 换域名，见 §0.2 约束 1 |
| Desktop 能连但功能缺失 | 远程 ACL 按设计拦截 | debug / fs-browse 远程 404 属预期，见 §9.4 |

---

## 12. 运维备忘

- **中继无状态**：只存设备公钥。崩溃 = Gateway 重连，`systemctl restart` 即恢复，不需要数据迁移。
- **零成本回滚**：`relay.enabled = false` 完全关闭隧道，Gateway 零出站连接。relay 服务本身可以留着。
- **日志**：`journalctl -u acowork-relay`（systemd 生命周期）；中继自身日志落在 `<data-dir>/logs/`；Gateway 侧在 `~/.acowork/acowork-gateway/data/logs`。
- **升级**：重新编译 → `sudo install -m 0755 <新二进制> /usr/local/bin/acowork-relay` → `systemctl restart acowork-relay`（Gateway 隧道自动重连）。
- **规模上限**（`RelayConfig::default()`）：单 Gateway 64 连接、待握手 256、全局隧道 5000、心跳 30s。
- **协议冻结**：Relay 冻结在 `proto = 1`（ADR-088 决策 10），此后只接安全修复。

---

## 13. 明确不可行的做法（不要尝试）

| 做法 | 为什么不行 |
|---|---|
| **裸 IP 代替域名** | 路由要求 `<gw-id>.<suffix>` 结构（§0.2 约束 1）。裸 IP 只能开控制面，**设备域永远匹配不上** |
| **用境外注册 / 无法备案的域名** | 备案走不通 → 境内 ECS 上会被云厂商拦截（§0.4），表现为"用几天就挂" |
| **换非标端口规避备案检测** | 既是规避性质、又丢掉 443 的防火墙友好性（设计 24 §5.2 选 443 的理由）。**不推荐** |
| **境外 ECS 自建** | 境内客户端 → 境外节点过国际出口，VPN/代理类长连接会被识别与重置；且涉及**跨境信道**合规问题。**在境内网络下不可用** |
| **家庭宽带对外提供 relay** | 运营商协议禁止对外提供服务；无公网 IPv4（多在 CGNAT 后）；入站 80/443 常被封；上行限速 |

---

## 附：与姊妹篇的选择

| 场景 | 用哪篇 |
|---|---|
| ECS 在**境内**（必须备案） | **本文** |
| ECS 在**境外** / 不需要备案 | [relay-ecs-cloudflare-deploy.md](./relay-ecs-cloudflare-deploy.md) |
| 只是想验证**软件链路**（与地域、备案无关） | 本地 / 同局域网即可：`service_domain = "127.0.0.1"`（[relay_full_path_e2e.rs:126](../../core/acowork-gateway/tests/relay_full_path_e2e.rs#L126) 即此形态） |