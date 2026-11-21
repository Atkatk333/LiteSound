# LiteSound

Windows 托盘的音频输出切换器：常驻系统托盘，左键 / 中键切到"上一台"输出设备，右键弹出设备菜单；单文件绿色部署，常驻私有内存约 2 MB 级。

## 特性

- **托盘即切换器**：左键 / 中键抬起切到上一台设备，右键弹出全部 active 端点的菜单。
- **自绘菜单**：单一 `WS_POPUP` 分层窗，整帧经 `UpdateLayeredWindow` 原子提交；圆角由逐像素 alpha 实现，不抓屏、不叠系统菜单窗。
- **绿色部署**：一个 exe，无安装包、无服务、无后台轮询；状态与诊断日志写在 exe 同目录（不可写时回退到 `%LOCALAPPDATA%\LiteSound`）。
- **低占用**：私有内存约 2 MB 级，线程数 ≤ 6，句柄增量受控。
- **状态记忆**：记住"上一台"设备；监听设备变更通知；explorer 重启后自动恢复托盘图标。
- **CLI 可用**：列表、诊断、切换与托盘模式共用同一套设备枚举与命名逻辑。

## 系统要求

- Windows 10 / 11。
- 运行时无需任何运行库；构建需要 Rust 工具链（MSVC 工具链 + Windows SDK）。
- 图标资源经 `build.rs` 编入 exe，需要 `rc.exe`（Windows SDK）或 PATH 上的 `windres`。

## 构建

```powershell
cargo build --release
```

产物为 `target\release\litesound.exe`；另含启动器 `litesound-launcher.exe`（GUI 子系统，供开机自启使用）。

依赖仅 `windows` 与 `windows-core`（`default-features = false`），不引入其他运行时 crate。

## 用法

```text
litesound <子命令>

  --list         列出全部渲染端点（含 disabled / not-present / unplugged）
  --props        打印每台端点的原始属性值，排查命名/分类问题用
  --menu         打印右键菜单内容（仅 active 端点）
  --set <ID>     按 ID 切换默认输出设备并回读校验
  --bannertest <文本>  只画一次横幅，5 秒后自动退出
  --tray         常驻托盘（等价于不带参数）
  --version      只打印版本号
  --help         打印用法
```

不带参数等同 `--tray`。托盘模式为单实例：已有实例在跑时新进程静默退出；CLI 子命令不受单实例限制。`--set` 成功与"已经是当前"都返回 0，切换失败返回 1，找不到 ID 返回 2。

## 已知限制

- 首次打开菜单比后续多约 9 ms（菜单帧表预热的固有成本，预优化为负收益，未做）。
- 开机自启走独立 GUI 启动器；主程序保持控制台子系统并在进入托盘前 `FreeConsole`，以保证 CLI 退出码可被 shell 读取。
- 菜单为分层窗自绘，层序按 TOPMOST 维护，但 Win32 前台锁与 TOPMOST 带内层序无绝对保证，极端情况下仍可能被其他窗口遮挡。
- 仅支持 Windows。

自启动从 Run 键拉起启动器到托盘图标可被 Shell 查询，实测中位约 138 ms（p10–p90 约 135–144 ms）；全新二进制首次登录会因安全软件首扫再多约 50 ms，本版未做首次登录的延迟优化。
