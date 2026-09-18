# 图标缓存重建工具 (IconCacheRebuilder)

基于 **Tauri 2 + Vue 3 + TypeScript + Vite 5** 的 Windows 桌面工具，用于重建 Windows 图标缓存，修复桌面 / 任务栏 / 资源管理器中图标显示异常（白图标、错位图标、缓存损坏）的问题。

## 功能说明

等价于传统的「重建图标缓存.bat」脚本，以图形界面方式执行以下四个步骤：

| 步骤 | 对应 bat 命令 | 说明 |
| --- | --- | --- |
| 1. 结束 explorer.exe | `taskkill /f /im explorer.exe` | 释放图标缓存文件句柄 |
| 2. 清理缓存目录 | `del /f /s /q %LOCALAPPDATA%\Microsoft\Windows\Explorer\*` | 递归删除 iconcache / thumbcache 等缓存文件 |
| 3. 删除 IconCache.db | `del /f /a %LOCALAPPDATA%\IconCache.db` | 清除只读 / 隐藏 / 系统属性后删除 |
| 4. 重启 explorer.exe | `start explorer.exe` | 任务栏与桌面自动恢复 |

特性：

- 执行前展示缓存概况（目录、文件数、总大小）
- 四个步骤实时展示执行状态（执行中 / 成功 / 警告 / 失败）
- 时间戳执行日志，失败文件逐条列出（如被其他进程占用）
- 执行前二次确认弹窗；重建运行期间禁止关闭窗口，避免 explorer 未重启
- 无需管理员权限（仅操作当前用户目录与进程）
- **在线升级**：启动时自动检查新版本，也可点击底部「检查更新」手动触发（GitHub Releases，官方源 + 国内镜像双通道）

## 环境要求

- [Node.js](https://nodejs.org/) >= 18
- [Rust](https://rustup.rs/)（stable-msvc 工具链）
- Microsoft C++ Build Tools（Visual Studio「使用 C++ 的桌面开发」工作负载）
- WebView2 Runtime（Windows 11 自带）

## 首次构建前：生成应用图标

项目的 `src-tauri/icons/` 需要应用图标文件。在项目根目录执行一次：

```powershell
npm install
npx tauri icon app-icon.png
```

`app-icon.png` 为 1024×1024 图标源文件（已提供），该命令会自动生成 Windows / macOS 全套图标。

## 开发调试

```powershell
npm run tauri dev
```

## 构建发布

```powershell
npm run tauri build
```

产物：

- 可执行文件：`src-tauri/target/release/icon-cache-rebuilder.exe`
- NSIS 安装包：`src-tauri/target/release/bundle/nsis/IconCacheRebuilder_0.1.0_x64-setup.exe`

### 关于 useLocalToolsDir

`tauri.conf.json` 中启用了 `bundle.useLocalToolsDir`，NSIS 打包工具集存放在 `src-tauri/target/.tauri/NSIS/`（已随项目构建目录就位）。好处：

- 打包时不再从 GitHub 下载 NSIS（本机网络访问 GitHub 超时时构建仍可完成）
- 工具集随项目目录隔离，`cargo clean` 后如需重新打包，可将该目录备份后放回，或删除 `useLocalToolsDir` 配置项改用全局目录（首次会自动下载）

> 说明：`src-tauri/Cargo.lock` 会在首次构建时自动生成（Rust 依赖从 crates.io 解析），不影响构建结果。

## 在线升级与发布流程

应用通过 `tauri-plugin-updater` 实现在线升级：启动时静默检查一次更新，也可点击底部「检查更新」手动触发。检查更新依次尝试 `tauri.conf.json > plugins > updater > endpoints` 中的地址（GitHub 官方源 → 国内镜像源），任一成功即用。

更新包强制签名校验：公钥（pubkey）已写入 `tauri.conf.json`；对应私钥由开发者保管，**绝不能提交进仓库**。

### 首次准备（一次性）

1. 在 GitHub 创建公开仓库（例如 `<你的用户名>/icon-cache-rebuilder`）
2. 将 `src-tauri/tauri.conf.json` 中两处更新源地址的仓库所有者替换为你的 GitHub 用户名（本项目已指向 `WX308806058/icon-cache-rebuilder`）
3. 在仓库 **Settings → Secrets and variables → Actions** 中新建 Secret：
   - 名称：`TAURI_SIGNING_PRIVATE_KEY`
   - 值：私钥文件 `icon-cache-rebuilder.key` 的完整内容（两行：`untrusted comment: minisign secret key...` 注释行 + base64 数据行）
4. 推送项目代码到仓库

私钥文件由 `npx tauri signer generate` 生成（本项目的密钥对生成于开发过程，请向开发者索取并妥善备份）。若私钥丢失，历史版本应用将无法通过自动更新升级到新版本，只能手动下载安装。

### 发布新版本

```powershell
git add .
git commit -m "release: v0.2.0"
git tag v0.2.0
git push origin main --tags
```

推送 `v*` 格式的 tag 后，GitHub Actions（`.github/workflows/publish.yml`）自动执行：

1. 从 tag 同步版本号（无需手动改 `tauri.conf.json` / `package.json`）
2. 构建 NSIS 安装包，并用 `TAURI_SIGNING_PRIVATE_KEY` 签名生成更新包
3. 上传 `xxx-setup.exe`、`xxx-setup.nsis.zip`（+ `.sig`）、`latest.json` 到草稿 Release
4. 到仓库 Releases 页确认无误后点 **Publish release** —— 已安装旧版本的应用即可检测到并完成升级

### 本地构建（可选）

```powershell
npm run tauri build
```

如需在本地生成带签名的更新包（用于验证升级流程），构建前设置环境变量：

```powershell
$env:TAURI_SIGNING_PRIVATE_KEY = Get-Content <私钥文件路径> -Raw
npm run tauri build
```

## 注意事项

- 执行重建时任务栏与桌面会短暂消失（约 1~3 秒），请先保存正在编辑的工作
- 已打开的资源管理器窗口会被关闭（explorer.exe 强制结束所致）
- 若部分缓存文件删除失败，通常是其他进程占用句柄，详见日志输出；重启系统后再次执行通常即可清理

## 项目结构

```
├── app-icon.png          # 图标源文件（1024×1024）
├── index.html            # Vite 入口
├── src/                  # Vue 3 前端
│   ├── App.vue           # 主界面（步骤展示 / 日志 / 确认弹窗）
│   ├── types.ts          # 与 Rust 侧共享的类型定义
│   ├── main.ts
│   └── style.css
└── src-tauri/            # Tauri 2 后端
    ├── src/lib.rs        # 核心命令：rebuild_icon_cache / get_cache_info
    ├── tauri.conf.json   # 窗口与打包配置
    └── capabilities/     # 权限声明
```
