# rc-relay：自建中转

只给你自己的设备用的中转：两台电脑直连打不通时，画面经它转发。中转只能看到加密后的字节，看不到画面、键鼠和命令内容。

---

## 它为谁转发

- **成员**：贴过中转码的电脑。贴同一个码的成员之间互相转发。一台中转可以开几个码，分给不同的人用，不同码之间不转发。
- **访客**：每台成员电脑报上来的设备，也就是它的我的电脑、信任设备和正在用的临时设备。访客只能和带它进来的那台成员互相转发。
- **其他人**：能连上中转，但什么也转不了。

在管理界面里删掉一台设备，1 秒内生效。删的是访客的话，带它进来的那台电脑也会把它删掉。

---

## 部署（Docker）

1. 在 Portainer、1Panel、Dockge 等面板里新建 Stack，粘贴 [deploy/docker-compose.yml](deploy/docker-compose.yml)；或者在服务器上执行 `docker compose up -d`。
2. 服务器防火墙和云厂商安全组放行 `7843/TCP`（必须）和 `7842/UDP`（帮电脑打洞直连，建议放行）。
3. 从 `docker logs rc-relay` 复制中转码，每台电脑都贴到软件的「设置 → 自建中转」。

卷 `/var/lib/rc-relay` 里存着中转码、成员和证书。迁移服务器时把它整个带走；丢了的话，所有电脑都要重新贴码。

---

## 地址和证书

默认什么都不用配：中转从网卡上找出服务器的公网 IPv4 和 IPv6（网卡上没有公网 IPv4 时，就问 `api.ipify.org`），再自己生成一张证书，把地址和证书指纹一起写进中转码。软件按指纹认这张证书，所以不需要域名，也不需要 Let's Encrypt。

| 情况 | 要做什么 |
| --- | --- |
| 有公网 IPv4 或 IPv6，或两种都有 | 什么都不用配。两种都有时，软件先试 IPv6，再试 IPv4；只有 IPv6 时，用的人也得有 IPv6 |
| IP 会变（家里的宽带、没绑固定 IP 的云服务器） | 用域名：在 compose 里加环境变量 `RC_RELAY_PUBLIC: "relay.example.com"`，DNS 加 A / AAAA 记录 |
| 想用正式证书 | 见下文 |

中转码里写的是 IP 时，**IP 变了，每台电脑都要重新贴码**；写的是域名时，改 DNS 记录就行。

### 用正式证书

不是必须的：默认的自签证书一样安全。正式证书的好处只是地址里不带指纹、浏览器能直接打开，代价是证书 90 天一张，要自动续期。

1. 用 certbot 为域名申请证书。验证方式可以用 HTTP-01（需要 80 端口能从外网访问），也可以用 DNS-01（通过 DNS 服务商的 API 验证，不需要 80 端口）。
2. 给 certbot 配一个 deploy hook，每次续期后把证书和私钥复制到 `/etc/rc-relay/tls`。
3. 写配置文件 `/etc/rc-relay/config.toml`，模板是 [deploy/config.example.toml](deploy/config.example.toml)：

   ```toml
   state_dir = "/var/lib/rc-relay"
   public_urls = ["https://relay.example.com:7843"]

   [tls]
   cert_mode = "Reloading"      # 每 24 小时重读一次证书，续期后不用重启
   manual_cert_path = "/etc/rc-relay/tls/cert.pem"
   manual_key_path = "/etc/rc-relay/tls/key.pem"
   ```

4. compose 里加上启动参数和挂载：

   ```yaml
   services:
     rc-relay:
       image: ghcr.io/peteroooooooo/rc-relay:latest
       container_name: rc-relay
       restart: unless-stopped
       network_mode: host
       command: ["--config", "/etc/rc-relay/config.toml", "serve"]
       volumes:
         - rc-relay:/var/lib/rc-relay
         - /etc/rc-relay/config.toml:/etc/rc-relay/config.toml:ro
         - /etc/rc-relay/tls:/etc/rc-relay/tls:ro

   volumes:
     rc-relay:
   ```

   容器默认以 uid 10001 运行，要能读私钥文件。可以在 deploy hook 里把文件的所有者改成这个用户，也可以用 `user:` 让容器以私钥文件所有者的身份运行。

### 其他情况

- **家里的机器，在路由器后面**：在路由器上把 `7843/TCP` 和 `7842/UDP` 转发到这台机器。如果路由器拿到的 WAN 地址在 `100.64.x.x` 段，说明运营商用了 CGNAT，IPv4 做不了中转，只能用 IPv6，还要在光猫或路由器上放行入站。
- **面板不允许 host 网络**：改成映射 `7843:7843/tcp` 和 `7842:7842/udp`，并用 `RC_RELAY_PUBLIC` 写明公网地址，因为容器里看不到服务器的公网地址。这种模式下 IPv6 基本用不了。
- **域名在 Cloudflare 上**：必须用灰云（只解析，不代理）。Cloudflare 的代理不转发 7843 端口和 UDP，还会换上它自己的证书。

---

## 管理界面与命令

在容器控制台执行 `rc-relay`（或者 `docker exec -it rc-relay rc-relay`），进入全屏管理界面：按中转码分组查看在线的成员和各自的访客，改动 1 秒内生效。只想看纯文本列表，执行 `rc-relay codes`。

| 命令 | 在哪跑 | 做什么 |
| --- | --- | --- |
| `rc-relay serve [--public …]` | 服务器 | 运行中转，启动时打印第一个中转码 |
| `rc-relay`（或 `rc-relay ui`） | 服务器 | 打开管理界面：`n` 新建码，`c` 复制或单行展示中转码，`r` 改名，`d` 删除成员或踢出访客（先确认），`u` 撤回删除的码或成员 |
| `rc-relay ui --demo` | 任意机器 | 用临时目录里的模拟数据试用管理界面，退出后自动清理 |
| `rc-relay codes` | 服务器 | 以纯文本打印所有中转码和各自名下的电脑 |
| `rc-relay claim <中转码> [--name 电脑名] [--key 设备密钥文件]` | 成员的电脑 | 加入中转（软件里"贴中转码"做的就是这个） |

中转码默认按最小的空闲编号命名为「码 1」「码 2」……，可以改名。

---

## 不用 Docker 时

```text
cargo build --release -p rc-relay            # Windows 上直接编
```

Linux 版在 WSL 里编，别在小内存 VPS 上编。编完用
`objdump -T rc-relay | grep -o 'GLIBC_[0-9.]*' | sort -uV | tail -1` 看一眼最高要求的 glibc 版本，不能高于服务器的。

systemd 部署参考：

- 程序 `/opt/rc-relay/rc-relay`，配置 `/etc/rc-relay/config.toml`（参考 [deploy/config.example.toml](deploy/config.example.toml)），状态 `/var/lib/rc-relay/`，服务 [deploy/rc-relay.service](deploy/rc-relay.service)。
- 用独立的系统用户跑服务时，管理界面也要用这个用户跑，免得状态文件变成 root 的：`sudo -u rc-relay /opt/rc-relay/rc-relay`。

## 在线检查

```text
cargo run --release -p rc-relay --example relay_check -- keygen owner.key   # 生成测试设备密钥
# 在中转机器上执行 rc-relay codes 拿一个中转码，然后让 owner.key 加入：
cargo run --release -p rc-relay -- claim <中转码> --key owner.key --name check
cargo run --release -p rc-relay --example relay_check -- run https://relay.example.com:7843 owner.key
```

检查会替换这个临时成员的名单，结束时清空；检查完记得在管理界面里把它移除。

---

## 内部细节（给开发者）

**状态文件**（`/var/lib/rc-relay/`）：

| 文件 | 内容 |
| --- | --- |
| `codes.txt` | 中转码分组 |
| `owners.txt` | 成员，记在各自的码下面 |
| `guests.txt` | 每台成员报上来的访客，带电脑名称和类别（我的电脑 / 信任设备） |
| `kicked.txt` | 已踢出、成员电脑还没处理的访客 |
| `invites.txt` | 成员还没用掉的连接码 |
| `status.txt` | 在线设备和临时见面，给管理界面看 |
| `tls/` | 自签证书 |
| `urls.txt` | 上次启动时算出的地址 |

**接口**：和 `/relay` 用同一个 HTTPS 端口。

| 接口 | 谁调用 | 做什么 |
| --- | --- | --- |
| `POST /rc/claim` | 新成员 | 用中转码加入 |
| `POST /rc/guests` | 成员 | 报整份访客名单，新的一份替换旧的。答复里带上中转踢掉的访客 |
| `POST /rc/invites` | 成员 | 报还没用掉的连接码（令牌和过期时间），整份替换 |
| `POST /rc/meet` | 新设备 | 带着连接码的令牌和自己的签名来；中转作废令牌，让它和这台成员互相转发 10 分钟，成员见到它后把它加进名单 |
| `POST /rc/watch` | 成员 | 一直开着；中转一踢人就说 `sync`，其他时候每 20 秒发一个空行 |
| `GET /ping` | 任何人 | 探活 |

**设备怎么算**：一台设备连上中转时，按它自己的设备码算；带了设备证明（`Authorization: Bearer rcd1…`，会话窗口的临时设备码）就按证明里的设备算。证明只在连上的那一刻核对有效期。
