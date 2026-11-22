**简体中文** · [English](README.en.md)

# rc-relay

Remote Control 的私有中转服务，仅转发自有设备的端到端加密流量：

- **成员**：粘贴了同一中转码的电脑，成员之间可互相转发（支持创建多个中转码分组，不同分组间相互隔离）。
- **访客**：成员电脑上报的「我的电脑」「信任设备」和当前「临时设备」，仅能与对应的成员电脑互相转发。

---

## Docker 部署

使用 [deploy/docker-compose.yml](deploy/docker-compose.yml)：

```yaml
services:
  rc-relay:
    image: ghcr.io/peteroooooooo/rc-relay:latest
    container_name: rc-relay
    restart: unless-stopped
    network_mode: host
    volumes:
      - rc-relay:/var/lib/rc-relay

volumes:
  rc-relay:
```

1. 放行 `7843/TCP`（中转与控制）和 `7842/UDP`（辅助打洞）。
2. 从 `docker logs rc-relay` 或 `docker exec -it rc-relay ui` 复制中转码，粘贴到客户端 **「设置 → 自建中转」**。

持久化数据（中转码、成员列表、自签证书）保存在 `/var/lib/rc-relay`。

---

## 地址、域名与证书

- **默认（固定公网 IPv4 / IPv6）**：零配置。自动探测公网 IP 并生成自签证书，地址与证书指纹编码在中转码中（服务器 IP 变更后需重新贴码）。
- **动态 IP / 绑定域名**：添加环境变量 `RC_RELAY_PUBLIC: "relay.example.com"`（若使用 Cloudflare DNS，需设为仅 DNS 灰云）。
- **非 `host` 网络模式**：映射 `7843:7843/tcp` 与 `7842:7842/udp`，并通过 `RC_RELAY_PUBLIC` 指定公网 IP 或域名。
- **自定义 TLS 证书**：创建 `/etc/rc-relay/config.toml`（参考 [deploy/config.example.toml](deploy/config.example.toml)）并挂载证书目录（容器默认以 `uid 10001` 运行，需具备私钥读取权限）：

  ```toml
  state_dir = "/var/lib/rc-relay"
  public_urls = ["https://relay.example.com:7843"]

  [tls]
  cert_mode = "Reloading"      # 定期重读证书，续期无需重启
  manual_cert_path = "/etc/rc-relay/tls/cert.pem"
  manual_key_path = "/etc/rc-relay/tls/key.pem"
  ```

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

---

## 管理界面与 CLI

容器内执行 `ui`（或宿主机执行 `docker exec -it rc-relay ui`）进入终端管理界面，移除设备或踢出访客 1 秒内生效。

| 命令 | 说明 |
| --- | --- |
| `rc-relay serve [--public …]` | 启动中转服务并打印初始中转码 |
| `rc-relay` / `rc-relay ui` | 打开终端管理界面（`n` 新建码，`c` 复制码，`r` 改名，`d` 删除/踢出，`u` 撤回） |
| `rc-relay ui --demo` | 使用模拟数据预览终端管理界面 |
| `rc-relay codes` | 纯文本打印所有中转码及名下设备 |
| `rc-relay claim <中转码> [--name 名称] [--key 密钥文件]` | 命令行将设备加入中转 |

---

## 二进制编译与调试

**编译与 systemd 部署：**

```bash
cargo build --release -p rc-relay
```

systemd 服务文件见 [deploy/rc-relay.service](deploy/rc-relay.service)，配置文件模板见 [deploy/config.example.toml](deploy/config.example.toml)。

**连通性自检：**

```bash
cargo run --release -p rc-relay --example relay_check -- keygen owner.key
cargo run --release -p rc-relay -- claim <中转码> --key owner.key --name check
cargo run --release -p rc-relay --example relay_check -- run https://relay.example.com:7843 owner.key
```

---

## 内部结构

**状态文件（`/var/lib/rc-relay/`）：**

| 文件 | 内容 |
| --- | --- |
| `codes.txt` | 中转码分组 |
| `owners.txt` | 各分组下的成员设备 |
| `guests.txt` | 成员上报的访客设备（含名称与类别） |
| `kicked.txt` | 已踢出、待成员端同步移除的访客 |
| `invites.txt` | 未使用的有效连接码令牌 |
| `status.txt` | 在线状态快照（供 TUI 读取） |
| `tls/` | 自签证书与私钥 |
| `urls.txt` | 最近一次启动解析出的公网地址 |

**HTTP 接口（与 `/relay` 共用同一 HTTPS 端口）：**

| 接口 | 调用方 | 说明 |
| --- | --- | --- |
| `POST /rc/claim` | 新成员 | 凭中转码注册为成员 |
| `POST /rc/guests` | 成员 | 全量同步访客名单，返回被踢出的访客列表 |
| `POST /rc/invites` | 成员 | 全量同步有效连接码令牌与过期时间 |
| `POST /rc/meet` | 新设备 | 校验并核销连接码令牌，授权与对应成员临时转发 10 分钟 |
| `POST /rc/watch` | 成员 | 长连接推送：踢人时立即下发 `sync`，空闲时每 20 秒心跳 |
| `GET /ping` | 任意 | 健康检查 |
