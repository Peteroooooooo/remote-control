# iroh 1.4.1 的改动

这里是 crates.io 上 iroh 1.4.1 的原样拷贝（去掉了 `.cargo-ok`、`.cargo_vcs_info.json` 和 `Cargo.lock`），
工作区根 `Cargo.toml` 用 `[patch.crates-io]` 让所有人都编译这一份。

升级 iroh 时：拿新版本重新拷一份，按下表把改动补回去，再跑 `cargo test -p remote-control`。
上游改掉了这个问题时，就不再需要这份拷贝。

| 文件 | 改动 | 为什么 |
|---|---|---|
| `src/socket/remote_map/remote_state.rs` | `handle_msg_add_connection`：发起方的新连接，第一条通路是中转时，也给已知的其他中转地址各开一条通路（原版只在第一条是直连时才开） | 一台设备可以同时在两台中转上：自己的公共节点，加上对方的自建中转。原版里第一个包经公共节点到达后，这条连接就再也不会经自建中转走，打不通直连时画面没法改走自建中转 |
| `src/socket/transports/relay/actor.rs` | `try_send_datagram`：直接按 `item.url` 调用 `active_relay_handle(url)`，不再经 `active_relay_handle_for_endpoint` 重定向到其他已有路由缓存的中转；加回归测试 `test_send_to_second_relay_does_not_redirect` | 访客端先经公共节点收到对方数据包后，公共节点的 `ActiveRelayActor` 会缓存对方路由；之后访客端主动开对方自建中转路径或保活连接时，原版会把发往自建中转的包暗中改发到公共节点，导致永远连不上自建中转且误判路径已打通（2026-11-26 实测） |
