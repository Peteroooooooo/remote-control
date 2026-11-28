# netdev 0.46.3 的改动

这里是 crates.io 上 netdev 0.46.3 的原样拷贝（去掉了 `.cargo_vcs_info.json`、`.github`、`.gitattributes`、`.gitignore` 和 `Cargo.lock`），
工作区根 `Cargo.toml` 用 `[patch.crates-io]` 让 iroh 的 netwatch 编译这一份。`remote-control` 和 `rc-relay` 用的 netdev 0.45 不受影响。

升级 netwatch / netdev 时：拿新版本重新拷一份，按下表把改动补回去，再编一次 iOS（`apps/rc-ios/scripts/build-core.sh`）。
上游或 libc 改掉了这个问题时，就不再需要这份拷贝。

| 文件 | 改动 | 为什么 |
|---|---|---|
| `src/os/darwin/ipv6_addr_flags.rs` | 非 macOS 的苹果平台用本地定义的 `in6_ifreq`（布局同 `<netinet6/in6_var.h>`，288 字节，有编译期断言），macOS 照旧用 `libc::in6_ifreq` | libc 0.2.191 只在 `target_os = "macos"` 下定义 `in6_ifreq`，原版在 iOS 上编不过（`cannot find type in6_ifreq in crate libc`），iOS 控制端因此无法构建 |
