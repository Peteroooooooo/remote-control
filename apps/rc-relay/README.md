# rc-relay：自建中转

在 iroh-relay 上加了转发规则的中转程序：

- **成员与访客规则**：贴过中转码的设备都是成员（`owners.txt`），成员之间以及成员与各自带来的访客都可以互相转发；通过连接码连进来的访客（`guests.txt` / 临时见面）**只能连带它进来的那台成员**，不能跨成员、也不能连其他访客。
- **黑名单（`blocked.txt`）**：在管理界面选中访客按 `d` 即可立即拦截，即使成员电脑仍在同步名单也不会转发，直到在左侧「已禁止」里解除。
- **中转码分组**：中转在 `codes.txt` 里维护一组或多组中转码（默认按最小空闲编号命名为 `码 1`、`码 2`…，可改名）。**同一个码可以给多台电脑重复贴**；谁贴了某个码，谁就成为成员并记在 `owners.txt` 对应的组下，直到在管理界面里把这台电脑移除或删掉整个码。
- 名单 = 成员 + 每台成员报上来的访客（`guests.txt`，带电脑名称）。成员的电脑自动维护自己那一份：它的设备列表，加上这次运行里用连接码见过的临时设备；每次有变化就把整份发给中转（`POST /rc/guests`），新的一份替换旧的。
- 一台设备连上中转时，按它自己的设备码算；带了设备证明（`Authorization: Bearer rcd1…`，会话窗口的临时设备码）就按证明里的设备算。证明只在连上的那一刻核对有效期。
- 谁都能连上中转，只是名单外的转不了；所以刚加进名单的设备不用重连，马上就能转。
- 新设备第一次见面：主人把还没用掉的连接码（令牌和过期时间）报给中转（`POST /rc/invites`，整份替换，存在 `invites.txt`）。新设备带着令牌来（`POST /rc/meet`，带它自己的签名），中转把令牌作废，让它和这台主人互相转发 10 分钟；主人见到它后把它加进名单。
- 加入（`POST /rc/claim`）、报名单（`POST /rc/guests`）、报连接码（`POST /rc/invites`）、带码见面（`POST /rc/meet`）和探活（`GET /ping`）跟 `/relay` 用同一个 HTTPS 端口。

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
- 卷挂在 `/var/lib/rc-relay`：中转码、主人、名单和自签证书都在里面。卷丢了证书就变了，电脑要重新贴中转码。
- 启动后 `docker logs rc-relay` 里会打印第一个中转码，你的每台电脑都贴这一个即可。
- 需要分组、看在线状态或踢人时，打开容器控制台执行 `rc-relay`（或 `docker exec -it rc-relay rc-relay`），会进入全屏管理界面；只看纯文本列表执行 `rc-relay codes`。
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
| `rc-relay serve [--public …]` | 服务器 | 运行中转，启动时打印第一个中转码 |
| `rc-relay`（或 `rc-relay ui`） | 服务器 | 打开全屏管理界面：按中转码分组查看在线成员与各自访客电脑名，支持 `n` 新建码、`r` 改名、`d` 删除成员或禁止/解禁访客；改动 2 秒内生效 |
| `rc-relay ui --demo` | 任意机器 | 用临时目录里的模拟数据试用管理界面（退出自动清理） |
| `rc-relay codes` | 服务器 | 以纯文本打印所有中转码和各自名下的电脑 |
| `rc-relay claim <中转码> [--name 电脑名] [--key 设备密钥文件]` | 主人的电脑 | 加入中转（软件里"贴中转码"做的就是这个） |

中转有几个地址时，客户端先连 IPv6，250 ms 后再同时试下一个，谁先答复用谁。

非容器部署、用独立系统用户跑服务时，管理界面要用服务的用户跑，免得状态文件变成 root 的：`sudo -u rc-relay /opt/rc-relay/rc-relay`。

## systemd 部署参考

- 程序 `/opt/rc-relay/rc-relay`，配置 `/etc/rc-relay/config.toml`（参考 `deploy/config.example.toml`），状态 `/var/lib/rc-relay/`（`codes.txt`、`owners.txt`、`guests.txt`、`blocked.txt`、`invites.txt`、`status.txt`），服务 `rc-relay.service`（`deploy/rc-relay.service`）。
- 端口：HTTPS 7843/TCP、QUIC 地址发现 7842/UDP。

## 在线检查

```text
cargo run --release -p rc-relay --example relay_check -- keygen owner.key   # 生成测试设备密钥
# 在中转机器上执行 rc-relay codes 拿一个中转码，然后让 owner.key 加入：
cargo run --release -p rc-relay -- claim <中转码> --key owner.key --name check
cargo run --release -p rc-relay --example relay_check -- run https://relay.example.com:7843 owner.key
```

检查会替换这个临时主人的名单，结束时清空；检查完记得在 `rc-relay` 界面里把它移除。
