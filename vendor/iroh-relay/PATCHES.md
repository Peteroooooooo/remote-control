# iroh-relay 1.4.1 的改动

这里是 crates.io 上 iroh-relay 1.4.1 的原样拷贝（去掉了 `.cargo-ok` 和 `Cargo.lock`），
工作区根 `Cargo.toml` 用 `[patch.crates-io]` 让所有人（包括 iroh 客户端那一侧）都编译这一份。
服务端只加了 rc-relay 需要的钩子，不改原有行为：不用这些钩子时，和原版完全一样。
客户端只改了拨号时 IPv4/IPv6 的先后（见表最后一行）。

升级 iroh 时：拿新版本重新拷一份，按下表把改动补回去，再跑 `cargo test -p rc-relay`。

| 文件 | 改动 | 为什么 |
|---|---|---|
| `src/server.rs` | `AccessControl` 加方法 `allow_forward(src, dst, len) -> bool`，默认返回 `true`；`DynAccessControl` 和它的通用实现跟着加同名方法 | 原版只能在连接时决定放不放行，rc-relay 要按名单逐个数据包决定转不转发（名单变了马上生效，不用断开重连） |
| `src/server/client.rs` | `handle_frame_send_packet` 转发前先问 `allow_forward`，返回 `false` 就丢弃（计入 `send_packets_dropped`） | 同上，这是钩子的调用点 |
| `src/server.rs` | `RelayConfig` 加字段 `handlers: Vec<(Method, &'static str, HyperHandler)>`（`RelayConfig::new` 里为空）；`Server::spawn` 把它们注册到 HTTPS 服务上 | 加入中转（`/rc/claim`）和报名单（`/rc/guests`）等接口要和 `/relay` 用同一个端口、同一张证书 |
| `src/server/http_server.rs` | `HyperHandler` 类型从 `pub(super)` 改成 `pub` | 外部要能构造上面的 handler |
| `src/server/testing.rs` | 测试用的 `RelayConfig` 字面量补上 `handlers: Vec::new()` | 加了字段后原测试代码要能编译 |
| `src/client/tls.rs` | `dial_happy_eyeballs`：另一种地址（IPv4/IPv6）的第一个地址不再等 250 ms，和首选的同时拨，谁先连上用谁；加测试 `other_family_races_from_the_start` | 原版刚启动时还没测出 IPv6 能用，总是先拨 IPv4；家里电信 IPv4 到新加坡中转 210 ms 还大量丢包，IPv6 只要 50 ms，但 IPv4 先拨先连上，整条中转就卡在 IPv4 上（2026-11-24 实测） |

其余客户端代码没有改。
