# 家宽 / 本地设备自建中转说明

不想买云服务器，可以直接把 `rc-relay` 跑在家里的树莓派、Linux 开发板或小服务器上。

省掉服务器租金的同时，需要先看清**适用范围**并完成下面几步**网络折腾**：

## 1. 先看适不适用（家里有公网 IPv4 vs 只有 IPv6）

- **家里有公网 IPv4**：做完桥接与端口映射后，对所有网络（包括纯 IPv4 公司内网）都有效，可直接替代云服务器。
- **家里只有公网 IPv6**：只能给**同样有 IPv6** 的外网设备中转。常见对称 NAT（NAT4）环境分情况：
  - **手机 4G / 5G 热点**：几乎都有 IPv6，可用。
  - **高校校园网**：多数有 IPv6，少数没有，看学校情况。
  - **公司 / 政企办公网**：绝大多数没有 IPv6，家里纯 IPv6 中转连不上，此类场景仍建议用带公网 IPv4 的云服务器。
- **本机拿到公网 IPv6**：运行中转的设备需直连光猫，或让下级路由器开启 IPv6 透传 / AP 模式，确保能拿到公网 IPv6 地址。

## 2. 放行光猫入站（二选一）

国内家宽光猫在默认路由模式下会拦住外网主动连入，必须处理这一步：

- **光猫改桥接（推荐）**：联系装维或自行改桥接，用自己的路由器拨号并放行 `7843/TCP`、`7842/UDP`。
- **超管放行防火墙**：用光猫超级管理员密码进后台，关闭 IPv6 防火墙，或仅对中转设备放行 `7843/TCP`、`7842/UDP`。

## 3. 绑定域名（推荐）

家宽重启或重拨后 IP 会变，建议配合 DDNS 绑定域名并设置 `RC_RELAY_PUBLIC`，避免每次 IP 变化都要在客户端重新贴中转码：

```yaml
services:
  rc-relay:
    image: ghcr.io/peteroooooooo/rc-relay:latest
    container_name: rc-relay
    restart: unless-stopped
    network_mode: host
    environment:
      RC_RELAY_PUBLIC: "relay.example.com"
    volumes:
      - rc-relay:/var/lib/rc-relay

volumes:
  rc-relay:
```

其余证书与管理命令见 [apps/rc-relay/README.md](../apps/rc-relay/README.md)。
