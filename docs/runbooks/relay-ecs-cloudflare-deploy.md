# acowork-relay 生产部署 — ECS + Cloudflare 泛域名

> 目标：`relay.acowork.ai`（控制面）+ `*.relay.acowork.ai`（设备域）部署到一台 Aliyun ECS。
> 依据：[设计文档 24 §5.3](../design/zh/24-cloud-relay-remote-access.md)（SNI 路由）与 [ADR-055 §6.13](../adr/zh/ADR-055-remote-runtime-node-topology.md)。
>
> **⚠️ 先确认 ECS 地域**：本文假设 ECS 在**境外**或不涉及 ICP 备案。若 ECS 在**中国大陆**且域名未完成
> ICP 备案，云厂商的周期性扫描会在部署数日后开始拦截 80/443（表现为「先能用、过几天挂掉」）。
> 境内场景请改用 [relay-selfhost-cn-deploy.md](./relay-selfhost-cn-deploy.md)。
>
> **本文按顺序执行，每步自带验证命令。验证不通过就别往下走** —— 后面的步骤都建立在前一步已通过的前提上。

---

## 0. 三条硬约束

违反任何一条，中继都不工作：

| # | 约束 | 原因 |
|---|------|------|
| 1 | `relay.acowork.ai` 和 `*.relay.acowork.ai` 都必须是 Cloudflare **仅 DNS（灰云）** | 橙云代理会在 Cloudflare 边缘终结 TLS，只回明文 HTTP。relay 靠 **TLS SNI** 判断是控制面还是设备隧道，SNI 到不了源站，无法路由 |
| 2 | 必须签发 `*.relay.acowork.ai` **通配符证书** | Desktop 访问 `<gw-id>.relay.acowork.ai`，每次 gw-id 不同，Desktop 校验 TLS。通配符只有 DNS-01 能签 |
| 3 | ECS 安全组放行 **443/TCP 入站** | relay 自己监听 443 |

---

## 1. Cloudflare DNS

Cloudflare → `acowork.ai` → **DNS** → **Add record**，加两条：

| Type | Name | Content | Proxy status |
|------|------|---------|--------------|
| `A` | `relay` | `<ECS 公网 IP>` | **DNS only（灰云）** |
| `A` | `*` | `<ECS 公网 IP>` | **DNS only（灰云）** |

- `relay` → 服务域（控制面）
- `*` → 泛解析，覆盖所有 `<gw-id>.relay.acowork.ai`

**验证**（等 1 分钟，然后在本机执行）：

```bash
dig +short relay.acowork.ai                    # 应返回 ECS 公网 IP
dig +short x.relay.acowork.ai                  # 应返回同一个 IP
```

两个都必须**直接是 ECS IP**，不能是 Cloudflare 边缘 IP（`104.x` / `172.64.x` 等）。

---

## 2. ECS 安全组

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

## 3. 系统初始化

以 **root 或 sudoer 账号**登录 ECS（`acowork-relay` 是后面给 systemd 用的 nologin 系统账号，不要用它登录）。

```bash
sudo useradd -r -s /usr/sbin/nologin acowork-relay
sudo install -d -o acowork-relay -g acowork-relay -m 0750 /var/lib/acowork-relay
```

**验证**：

```bash
id acowork-relay        # 应显示 uid，且 shell 是 /usr/sbin/nologin
```

---

## 4. 装 certbot

**先看源里有没有打包好的 certbot**（有就走路线 A，没有走 B）：

```bash
sudo dnf list --available 'certbot*' 'python3-certbot-dns-cloudflare'
```

### 路线 A：有包（推荐）

RPM 依赖由发行版解决，**全程不调 pip**，不会触发 §11 的 pip 缺陷。

```bash
sudo dnf install -y certbot python3-certbot-dns-cloudflare
certbot --version
```

### 路线 B：无包 → venv

```bash
sudo python3 -m venv /opt/certbot
sudo /opt/certbot/bin/pip install -U pip setuptools wheel   # ← 不能省，见 §11
sudo /opt/certbot/bin/pip install -U certbot certbot-dns-cloudflare
sudo ln -sf /opt/certbot/bin/certbot /usr/bin/certbot
```

> ⚠️ **路线 A 和 B 不能混装。** 走了 A 就不要再执行 B 的 `ln -sf`，否则 venv 里的 certbot 找不到装在系统目录的插件，报 `Could not choose appropriate plugin`。

**验证**（两个路线都要跑）：

```bash
certbot --version
certbot plugins 2>/dev/null | grep -i cloudflare    # 必须有 dns_cloudflare 一行
```

没有 `dns_cloudflare` 就别往下走 —— 后面签发必然失败。

---

## 5. Cloudflare API Token

Cloudflare → My Profile → **API Tokens** → Create Token → **Edit zone DNS** 模板：

- **Permissions**：`Zone / DNS / Edit` **＋ `Zone / Zone / Read`**（certbot 要读 zone 找 zone id，缺了签发必失败）
- **Zone Resources**：Include → Specific zone → `acowork.ai`

三个 TTL 字段：

| 字段 | 填法 | 填错的后果 |
|------|------|-----------|
| **Start Date** | **留空**（立即生效） | Cloudflare 按 **UTC 零点**解释。北京时间填「今天」要等到当天 08:00 才生效，期间所有调用返回笼统的 `9109 Invalid access token` |
| **Expiration / TTL** | **1 年**（上限） | 设 90 天会静默炸：证书 90 天、第 60 天续第一次、第 120 天续第二次；token 若 90 天过期，第二次续期 403，证书过期后不再续 |
| **Client IP Filtering** | **留空**（除非 ECS 绑了固定 EIP） | 非 EIP 的公网 IP 会变，一变 `renew` 直接 403 |

**写进文件**（静默读入，不进 `bash_history`，且吃掉粘贴自带的尾部换行）：

```bash
sudo install -d -m 0750 /etc/letsencrypt/api-credentials
sudo sh -c 'read -rs T && printf "dns_cloudflare_api_token = %s\n" "$T" > /etc/letsencrypt/api-credentials/cloudflare.ini'; sudo chmod 600 /etc/letsencrypt/api-credentials/cloudflare.ini
```

执行后终端无回显，粘贴 token 再回车。

**验证**：

```bash
sudo cat -A /etc/letsencrypt/api-credentials/cloudflare.ini
# 期望：dns_cloudflare_api_token = cfut_xxxx...$   （$ 紧贴 token，中间无空格、无 ^M）
```

```bash
TOKEN=$(sudo sed -n 's/^dns_cloudflare_api_token[[:space:]]*=[[:space:]]*//p' /etc/letsencrypt/api-credentials/cloudflare.ini)
curl -s -H "Authorization: Bearer $TOKEN" "https://api.cloudflare.com/client/v4/zones?name=acowork.ai" | head -c 300
unset TOKEN
# 期望：{"success":true,...} 且能看到 acowork.ai
```

> ⚠️ **必须用 `/zones` 验证，不要用 `/user/tokens/verify`。** 后者不校验时间窗，token 未生效时照样返回 `status:active`，会骗过你。

---

## 6. 签发通配符证书

```bash
sudo certbot certonly --dns-cloudflare --dns-cloudflare-credentials /etc/letsencrypt/api-credentials/cloudflare.ini -d relay.acowork.ai -d '*.relay.acowork.ai'
```

**验证**：

```bash
sudo certbot certificates
# 期望看到 relay.acowork.ai + *.relay.acowork.ai 两个域名，Expiry 90 天
```

```bash
echo | openssl x509 -in /etc/letsencrypt/live/relay.acowork.ai/fullchain.pem -noout -text | grep -A1 "Alternative Name"
# 期望：DNS:relay.acowork.ai, DNS:*.relay.acowork.ai
```

---

## 7. 证书可读权限（relay 以非 root 运行）

unit 里 relay 以 `User=acowork-relay` 运行，**读不到 root 私钥服务就起不来**。

certbot 的目录结构里 `live/` 和 `archive/` 两个父目录都是 **0700 root-only**：

```
/etc/letsencrypt/                       0755
  live/                                 0700  ← relay 用户进不去
    relay.acowork.ai/                  0755
      privkey.pem -> ../../archive/relay.acowork.ai/privkey1.pem
  archive/                              0700  ← 也进不去
    relay.acowork.ai/                  0755
      privkey1.pem                      0640
```

必须给两个父目录 `x`（进目录不需要 `r`），给文件 `r`：

```bash
sudo setfacl -m u:acowork-relay:x /etc/letsencrypt/live
sudo setfacl -m u:acowork-relay:x /etc/letsencrypt/archive
sudo setfacl -m u:acowork-relay:r /etc/letsencrypt/live/relay.acowork.ai/privkey.pem
sudo setfacl -m u:acowork-relay:r /etc/letsencrypt/live/relay.acowork.ai/fullchain.pem
# 续期后新文件（privkey2.pem）自动继承
sudo setfacl -m d:u:acowork-relay:r /etc/letsencrypt/archive/relay.acowork.ai
```

**验证**（这一条不过，服务一定起不来）：

```bash
sudo -u acowork-relay head -c 1 /etc/letsencrypt/live/relay.acowork.ai/privkey.pem && echo "← privkey OK"
sudo -u acowork-relay head -c 1 /etc/letsencrypt/live/relay.acowork.ai/fullchain.pem && echo "← fullchain OK"
```

诊断用（逐级显示路径权限，挡路的那一级一眼可见）：

```bash
namei -l /etc/letsencrypt/live/relay.acowork.ai/privkey.pem
```

---

## 8. 编译 acowork-relay

在**本地**编译（Windows 用 WSL2、macOS 或 Linux 都行），不要在 ECS 上装 Rust 工具链。

先确认 ECS 架构（Aliyun 神龙/倚天实例是 **arm64**）：

```bash
# 在 ECS 上跑
uname -m          # x86_64 或 aarch64
```

对应关系：

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

# cargo 也要换源（tokio/axum/rustls 整棵树从 crates.io 拉）
mkdir -p ~/.cargo && printf '[source.crates-io]\nreplace-with = "rsproxy-sparse"\n\n[source.rsproxy-sparse]\nregistry = "sparse+https://rsproxy.cn/index/"\n\n[net]\ngit-fetch-with-cli = true\n' > ~/.cargo/config.toml
```

**编译**：

```bash
# 建议 clone 到本地文件系统（不是 /mnt/d/...），跨文件系统构建慢很多
cd ~/acowork
cargo build --release -p acowork-relay --target x86_64-unknown-linux-musl --manifest-path core/Cargo.toml
```

**验证**：

```bash
file core/target/x86_64-unknown-linux-musl/release/acowork-relay
# 期望：ELF 64-bit LSB executable, x86-64, statically linked, ...
```

必须显示 `statically linked`。若显示 `dynamically linked` + `interpreter /lib64/ld-linux-x86-64.so.2`，说明 musl 没生效 —— 产物在 Anolis 8（glibc 2.28）上可能引用高版本符号而跑不起来。

**上传**（二进制 **和** unit 都要传）：

```bash
scp core/target/x86_64-unknown-linux-musl/release/acowork-relay root@<ECS公网IP>:/tmp/
scp dev/deploy/relay/acowork-relay.service root@<ECS公网IP>:/tmp/
```

---

## 9. 部署 systemd 服务

```bash
# 二进制 + unit 就位
sudo install -m 0755 /tmp/acowork-relay /usr/local/bin/acowork-relay
sudo cp /tmp/acowork-relay.service /etc/systemd/system/
sudo install -d -o acowork-relay -g acowork-relay -m 0750 /var/lib/acowork-relay

# 改域名（模板里是 relay.example.com，4 处）
sudo sed -i 's/relay\.example\.com/relay.acowork.ai/g' /etc/systemd/system/acowork-relay.service
```

**验证替换到位**（4 个 `relay.acowork.ai` + 2 个文件路径）：

```bash
sudo grep -E 'service-domain|device-domain-suffix|tls-cert|tls-key' /etc/systemd/system/acowork-relay.service
```

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
| `Permission denied (os error 13)` | 证书 ACL 没给全 → 回 §7 |
| `Address already in use` | 443 被占 → `sudo ss -tlnp \| grep :443` |
| `No such file or directory` + 域名 | `sed` 没替换成功 → 重跑上面的 `grep` |
| `depends on a target or service not named` | `/var/lib/acowork-relay` 不存在或属主不对 → 重跑 `install -d` |

---

## 10. 端到端验证

### 10.1 relay 对外可用

```bash
curl -sS https://relay.acowork.ai/health          # 期望：ok
```

```bash
echo | openssl s_client -connect relay.acowork.ai:443 -servername relay.acowork.ai 2>/dev/null \
  | openssl x509 -noout -text | grep -A1 "Alternative Name"
# 期望：DNS:relay.acowork.ai, DNS:*.relay.acowork.ai
```

### 10.2 证书续期 hook

中继**不热加载**证书，续期后必须重启才生效：

```bash
sudo mkdir -p /etc/letsencrypt/renewal-hooks/deploy
sudo tee /etc/letsencrypt/renewal-hooks/deploy/reload-relay.sh >/dev/null <<'SH'
#!/bin/sh
logger -t acowork-relay-renew "cert renewed, restarting acowork-relay"
systemctl restart acowork-relay
SH
sudo chmod +x /etc/letsencrypt/renewal-hooks/deploy/reload-relay.sh
```

```bash
sudo certbot renew --dry-run                    # 验证 ACME 握手 + DNS-01 挑战（打 LE staging，不占生产额度）
sudo /etc/letsencrypt/renewal-hooks/deploy/reload-relay.sh   # 单独验 hook（会真重启中继）
journalctl -t acowork-relay-renew --since "-2 min" --no-pager   # 应看到 logger 那行
```

> ⚠️ `--dry-run` **不执行** deploy hook（certbot 官方行为）。dry-run 全绿不代表 hook 能用，所以要单独验。

### 10.3 Gateway 侧开启隧道

在你的 Gateway 机器上，`~/.acowork/acowork-gateway/config/gateway.toml`：

```toml
[relay]
enabled = true
url = "wss://relay.acowork.ai/tunnel"
```

或运行时启用：

```bash
curl -X POST http://127.0.0.1:19876/api/relay/enable -H 'Content-Type: application/json' -d '{"url":"wss://relay.acowork.ai/tunnel"}'
```

**验证**：

```bash
curl -s http://127.0.0.1:19876/api/relay/status | python3 -m json.tool
# 期望：{"enabled":true, "connected":true, "gw_id":"<UUID v4>"}
```

首次连接时 Gateway 会在 `~/.acowork/acowork-gateway/config/relay_identity.json` 生成 Ed25519 设备身份（私钥 **永不上传**）。

### 10.4 Desktop 远程连接

1. Desktop 设置 → 连接模式选 **Remote / Relay**
2. Gateway URL 填 `https://<gw-id>.relay.acowork.ai`（`<gw-id>` 来自 10.3 的 `/api/relay/status`）
3. 正常账号登录

> Debug 端点远程返回 404 属**预期**（远程 ACL 加固）：relay 模式自动 disables DevMode、LSP Relay 显示不可用（设计文档 24 §8.2 F7）。剪贴板文件、附件、技能导入走字节流上传。

---

## 11. 故障排查

| 症状 | 原因 | 处理 |
|------|------|------|
| certbot 装不上，报 `No module named 'setuptools_rust'` | venv 自带 pip 9.0.3（Alibaba Cloud Linux 3 / Anolis 8 的 py3.6）不认 PEP 513 的 `manylinux_2_17` wheel tag，回退源码编译；且 pip 9 无 PEP 517 build isolation | §4 路线 B：先 `pip install -U pip` 再装。**不要**去补装 `setuptools_rust`（要拖 Rust 编译器） |
| certbot 报 `9109 Invalid access token` | ① token 未到 Start Date（UTC 零点）② 权限缺 `Zone/Zone/Read` ③ token 有尾部空格 | §5 的两条验证：`cat -A` 查隐藏字符、`curl /zones` 查真实有效性。**别用** `/user/tokens/verify`（不校验时间窗） |
| certbot 报 `Could not choose appropriate plugin` | 路线 A/B 混装，venv 看不到 RPM 插件 | 重装：走哪条路线就用哪条路线的 certbot |
| `Permission denied (os error 13)` | `live/` `archive/` 是 0700，relay 用户进不去 | §7：给两个父目录 `x`，给文件 `r` |
| `curl /health` 超时 | 灰云没配 / 安全组 443 没开 / 服务没起 | `dig +short relay.acowork.ai` 必须返回 ECS IP；`systemctl status` |
| `/health` 通但设备域 502 | Gateway 隧道未建立 | Gateway 日志 + `curl /api/relay/status` |
| Desktop TLS 报错 | 证书缺通配符 SAN | 重跑 §6，确认 SAN 含两个域名 |
| 证书 60 天后失效 | deploy hook 断了 | §10.2 单独验 hook。**不要**用 `--force-renewal` 试（LE 对相同域名集合限流 5 张/周） |
| Desktop 能连但功能缺失 | 远程 ACL 按设计拦截 | debug/fs-browse 远程 404 属预期，见 §10.4 |

---

## 12. 运维备忘

- **中继无状态**：只存设备公钥。崩溃 = Gateway 重连，`systemctl restart` 即恢复，不需要数据迁移。
- **零成本回滚**：`relay.enabled = false` 完全关闭隧道，Gateway 零出站连接。relay 服务本身可以留着。
- **日志**：`journalctl -u acowork-relay`；Gateway 侧 `~/.acowork/acowork-gateway/data/logs`.
- **升级**：重新编译 → `sudo install -m 0755 <新二进制> /usr/local/bin/acowork-relay` → `systemctl restart acowork-relay`（Gateway 隧道自动重连）。
- **规模上限**（`RelayConfig::default()`）：单 Gateway 64 连接、待握手 256、全局隧道 5000、心跳 30s。
