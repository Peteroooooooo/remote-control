# rc-relay：自建中转

在 iroh-relay 上加了转发规则的中转程序：

- 只转发名单上的设备，而且每条连接必须有一端是主人；名单外的设备一个包都不转，没有主人时什么都不转发。不限速。
- 名单 = 主人 + 每台主人报上来的访客（`guests.txt`）。主人的电脑自动维护自己那一份：它的设备列表，加上这次运行里用连接码见过的临时设备；每次有变化就把整份发给中转（`POST /rc/guests`），新的一份替换旧的。
- 主人 = `owners.txt` 里的设备（认领的，或者用 `owners add`、中转码加的），加上主人在名单上标成 `mine` 的设备（它的「我的电脑」），一层层往下算。标记去掉，或者给标记的那台不再是主人，它就不再是主人。旧版软件报的名单不带标记，照样能读。
- 一台设备连上中转时，按它自己的设备码算；带了设备证明（`Authorization: Bearer rcd1…`，会话窗口的临时设备码）就按证明里的设备算。证明只在连上的那一刻核对有效期。
- 谁都能连上中转，只是名单外的转不了；所以刚加进名单的设备不用重连，马上就能转。
- 新设备第一次见面：主人把还没用掉的连接码（令牌和过期时间）报给中转（`POST /rc/invites`，整份替换，存在 `invites.txt`）。新设备带着令牌来（`POST /rc/meet`，带它自己的签名），中转把令牌作废，让它和这台主人互相转发 10 分钟；主人见到它后把它加进名单。
- 认领（`POST /rc/claim`）、添加主人（`POST /rc/add-owner`）、报名单（`POST /rc/guests`）、报连接码（`POST /rc/invites`）、带码见面（`POST /rc/meet`）和探活（`GET /ping`）跟 `/relay` 用同一个 HTTPS 端口。

## 零配置

不给配置文件，中转按默认值跑：

- 端口：HTTPS 7843（TCP），QUIC 地址发现 7842（UDP）。
- 地址：启动时从网卡上找一个公网 IPv6 和一个公网 IPv4；网卡上没有公网 IPv4（AWS 这类 1:1 NAT、容器里）就问 `api.ipify.org`。找到几个，中转码里就带几个（最多两个，先 IPv6）。也可以用 `serve --public <IP 或域名>`（可写多个，逗号隔开；环境变量 `RC_RELAY_PUBLIC`）直接指定。
- 证书：第一次启动自己生成一张（`<状态目录>/tls/`），以后一直用这张。证书的指纹写在中转地址里（`https://203.0.113.7:7843/?rc-cert=…`），跟着中转码、连接码、设备之间的招呼走；软件见过这个地址，就只为这个主机认这张证书，别的证书照常按公共根证书检查。所以不需要域名，也不需要 Let's Encrypt。
- 状态目录：Linux `/var/lib/rc-relay/`。

有域名和证书时用配置文件（`--config`，写法见 `src/config.rs` 开头的例子；仓库提供 `deploy/config.example.toml` 模板），`cert_mode = "Reloading"` 每天重读证书，续期后不用重启。

## Docker（推荐：容器面板直接部署）

提供自动构建的 `linux/amd64` 与 `linux/arm64` 多架构镜像 `ghcr.io/peteroooooooo/rc-relay:latest`。

在 Portainer、1Panel、Dockge 等面板里新建 Stack，直接粘贴 [deploy/docker-compose.yml](deploy/docker-compose.yml) 即可拉取运行（或用 `docker compose up -d`）：

- `network_mode: host`：中转要看到服务器自己的 IPv4 和 IPv6，两种都能直接收；若所在平台限制 host 网络，可改为映射 `7843/tcp` 和 `7842/udp` 并设置环境变量 `RC_RELAY_PUBLIC`。
- 卷挂在 `/var/lib/rc-relay`：主人、名单、认领码和那张自签证书都在里面。卷丢了证书就变了，主人们要重新贴中转码。
- 第一次启动后 `docker logs rc-relay` 里有中转码；管理主人用 `docker exec rc-relay rc-relay owners list`。
- 防火墙放行 7843/TCP 和 7842/UDP。

如果希望从源码自己构建镜像，在工作区根目录执行：

```text
docker build -f apps/rc-relay/Dockerfile -t ghcr.io/peteroooooooo/rc-relay:latest .
docker save ghcr.io/peteroooooooo/rc-relay:latest | ssh <服务器> docker load
```

## 不用 Docker 时

```text
cargo build --release -p rc-relay            # Windows 上直接编
```

Linux 版在 WSL 里编，别在小内存 VPS 上编。编完用
`objdump -T rc-relay | grep -o 'GLIBC_[0-9.]*' | sort -uV | tail -1` 看一眼最高要求的 glibc 版本，不能高于服务器的。

## 命令

| 命令 | 在哪跑 | 做什么 |
|---|---|---|
| `rc-relay serve [--public …]` | 服务器 | 运行中转；还没主人时打印中转码 |
| `rc-relay reset-claim` | 服务器 | 打印新的中转码，旧的作废（认领的那台电脑丢了或重装了时用） |
| `rc-relay owners list` / `add <设备码> [--note 备注]` / `remove <设备码>` | 服务器 | 查看主人（带各自名单上几台；靠「我的电脑」当上的主人标明是谁的），增删 `owners.txt` 里的主人；删掉的主人连同它的名单一起作废，靠它当主人的设备跟着取消；运行中的中转 2 秒内生效 |
| `rc-relay claim <中转码> [--key 设备密钥文件]` | 主人的电脑 | 认领中转（软件里"贴中转码"做的就是这个） |
| `rc-relay add-owner <中转地址,…> [--key …]` | 已是主人的电脑 | 生成给另一台电脑用的中转码（24 小时内、只能用一次）。软件里用不到：「我的电脑」自动成为主人 |

中转有几个地址时，客户端先连 IPv6，250 ms 后再同时试下一个，谁先答复用谁。

服务器上改主人要用服务的用户跑，免得文件变成 root 的：`sudo -u rc-relay /opt/rc-relay/rc-relay owners list`。

## systemd 部署参考

- 程序 `/opt/rc-relay/rc-relay`，配置 `/etc/rc-relay/config.toml`（参考 `deploy/config.example.toml`），状态 `/var/lib/rc-relay/`（`owners.txt`、`guests.txt`、`invites.txt`、`claims.txt`），服务 `rc-relay.service`（`deploy/rc-relay.service`）。
- 端口：HTTPS 7843/TCP、QUIC 地址发现 7842/UDP。

## 在线检查

```text
cargo run --release -p rc-relay --example relay_check -- keygen owner.key   # 打印设备码，临时加成主人
cargo run --release -p rc-relay --example relay_check -- run https://relay.example.com:7843 owner.key
```

检查会替换这个临时主人的名单，结束时清空；检查完记得 `owners remove` 掉这个临时主人。
