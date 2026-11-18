# desktop-probe 测试

## 这是在测什么

远程控制别人的电脑时，有三种画面普通程序看不到、也点不了：

- UAC 弹窗（"你要允许此应用对你的设备进行更改吗？"）
- 锁屏和密码 / PIN 输入框
- Ctrl+Alt+Del 界面

Windows 把这些画面放在一个单独的"安全桌面"上。RustDesk、Sunshine 能操作它们，是因为装了一个以系统身份（SYSTEM）运行的服务。这个测试就是确认我们的做法也行：装上服务后，能截到这三种画面，并且能点、能按键。做不到的话，远程时一弹 UAC 就只能干等。

测完会自动卸载，电脑上不留东西。

## 你要做的

1. 在资源管理器里打开 `tools\remote-control\spikes\desktop-probe\`，双击 `run-test.cmd`。
2. 弹出 UAC 时点"是"。这是让测试程序拿到管理员权限，只需要点这一次。
3. 之后程序会自己弹出第二个 UAC、自己点"是"，再自己打开并关闭 Ctrl+Alt+Del 界面。这段时间别动鼠标和键盘，大约 30 秒。
4. 程序提示"5 秒后锁屏"后，屏幕会锁上，程序会自动点一下屏幕让输入框出现。这时你**自己输入密码或 PIN 解锁**。
5. 解锁后窗口里会显示每一步"通过"还是"失败"，按回车关闭。

不用发截图或日志给我。结果在 `measurements-raw\uac-<时间>\` 文件夹里（`result.txt`、`probe.log`、`shots\`），我直接读。

## 程序自动做了什么

| 步骤 | 做法 | 怎么算通过 |
|---|---|---|
| 1 安装 | 装服务，服务在你的会话里启动辅助进程 | 服务在运行，辅助进程 20 秒内就绪 |
| 2 UAC | 以你的普通权限请求提权，弹出 UAC；用 UI 自动化找到"是"按钮的位置，再用模拟鼠标点它 | 15 秒内切到安全桌面；点击后，被提权的进程确实以管理员身份运行 |
| 3 Ctrl+Alt+Del | 服务发送 Ctrl+Alt+Del，2 秒后模拟按 Esc | 界面出现，按 Esc 后回到桌面 |
| 4 锁屏 | 锁屏，3 秒后模拟点击屏幕中间，等你解锁 | 锁屏出现在安全桌面上，3 分钟内解锁 |
| 5 收集 | 复制日志和安全桌面上的截图 | 至少有一张安全桌面截图 |
| 6 卸载 | 删除服务、程序文件、数据文件夹，恢复 Ctrl+Alt+Del 策略 | 都不在了，策略和安装前一样 |

## 可选：重启后的登录界面

这一项要重启电脑，所以没放进自动测试。想测的话，在管理员 PowerShell 里：

1. `.\target\release\desktop-probe.exe install`
2. 重启，在登录界面停 30 秒再登录。
3. `.\target\release\desktop-probe.exe status`，看 `C:\ProgramData\desktop-probe\shots\` 里有没有登录前的 `_Winlogon.png`。
4. `.\target\release\desktop-probe.exe uninstall`

## 出问题时怎么彻底清理

先试 `desktop-probe.exe uninstall`（管理员）。不行再在管理员 PowerShell 里依次执行（报"找不到"可以忽略）：

```powershell
sc.exe stop desktop-probe
sc.exe delete desktop-probe
Stop-Process -Name desktop-probe -Force -ErrorAction SilentlyContinue
Remove-Item -LiteralPath 'C:\Program Files\desktop-probe' -Recurse -Force -ErrorAction SilentlyContinue
Remove-Item -LiteralPath 'C:\ProgramData\desktop-probe' -Recurse -Force -ErrorAction SilentlyContinue
# 恢复 Ctrl+Alt+Del 策略：先看安装前的备份
Get-ItemProperty -LiteralPath 'HKLM:\SOFTWARE\desktop-probe' -ErrorAction SilentlyContinue
# 备份里有 PrevSASMissing=1：说明原来没有这个值，删掉即可
Remove-ItemProperty -LiteralPath 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System' -Name SoftwareSASGeneration -ErrorAction SilentlyContinue
# 备份里是 PrevSAS=N：改成用下面这行恢复原值（N 换成备份里的数字），不要执行上面那行删除
# Set-ItemProperty -LiteralPath 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System' -Name SoftwareSASGeneration -Value N
Remove-Item -LiteralPath 'HKLM:\SOFTWARE\desktop-probe' -Recurse -ErrorAction SilentlyContinue
```
