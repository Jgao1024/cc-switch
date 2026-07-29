# macOS 本地打包教程

本文档说明如何在本机（Apple Silicon / macOS）将 CC Switch 打包为 `.app` 与 `.dmg`，供自己测试使用。**不涉及 Apple 签名与公证**，适用于本机安装体验，不适合对外分发。

## 一键命令

已装好 Node / pnpm / Rust 环境的前提下，仓库根目录直接执行：

```bash
export PATH="$HOME/.cargo/bin:$PATH" && \
pnpm install && \
sed -i '' 's/"createUpdaterArtifacts": true/"createUpdaterArtifacts": false/' src-tauri/tauri.conf.json && \
pnpm tauri build; \
sed -i '' 's/"createUpdaterArtifacts": false/"createUpdaterArtifacts": true/' src-tauri/tauri.conf.json
```

说明：
- 临时关闭 `createUpdaterArtifacts`（本地自用打包不需要更新签名私钥），构建完自动改回 `true`，不会污染仓库配置。
- 用 `;` 而不是 `&&` 连接最后一步，保证即使 build 失败也会把配置改回来。
- 首次执行如果卡在 `pnpm install` 的 `[ERR_PNPM_IGNORED_BUILDS]`，先跑一次 `pnpm approve-builds esbuild msw` 再重新执行上面的命令。
- 产物在 `src-tauri/target/release/bundle/macos/CC Switch.app` 和 `.../bundle/dmg/*.dmg`（见下方"产物位置"）。
- 只想打 Intel 版或通用二进制，把 `pnpm tauri build` 换成 `pnpm tauri build --target x86_64-apple-darwin` 或 `--target universal-apple-darwin`。

下面是分步说明，遇到问题时按步骤排查。

## 前置环境

确认以下工具已安装：

```bash
node -v        # v20+ 均可，CI 用 v20
pnpm -v        # 10.x
rustc --version
xcode-select -p   # 需已安装 Xcode Command Line Tools
```

若 `cargo`/`rustc` 不在默认 `PATH` 中，先执行：

```bash
export PATH="$HOME/.cargo/bin:$PATH"
```

安装前端依赖（仓库根目录）：

```bash
pnpm install
```

首次执行如果卡在 `[ERR_PNPM_IGNORED_BUILDS]`（esbuild / msw 等依赖的 postinstall 脚本被 pnpm 默认拦截），批准一次即可：

```bash
pnpm approve-builds esbuild msw
```

## 关键配置：`createUpdaterArtifacts`

`src-tauri/tauri.conf.json` 中：

```json
"bundle": {
  "createUpdaterArtifacts": true
}
```

这个开关要求 Tauri 用私钥对更新包签名（`.tar.gz.sig`），私钥来自环境变量 `TAURI_SIGNING_PRIVATE_KEY`（配套 `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`）。本机没有配置这对密钥时，直接 `pnpm tauri build` 会在签名阶段报错失败。

**本地自用打包**，临时关闭该开关即可：

```json
"createUpdaterArtifacts": false,
```

打包完成后记得改回 `true` 再提交/推送，避免污染仓库配置（这是给正式发布用的开关，不应该留在关闭状态）。

> 如果需要生成带自动更新签名的正式产物，见下方"完整签名 + 公证流程"一节。

## 打包命令

在仓库根目录执行：

```bash
pnpm tauri build
```

该命令会依次：
1. 跑 `beforeBuildCommand`（`vite build`，构建前端产物到 `dist/`）
2. 用 `cargo build --release` 编译 Rust 后端（首次编译较慢，约 3-5 分钟；有缓存后明显加快）
3. 打包成 `.app`，再生成 `.dmg`

只打某个目标架构（例如要打 Intel 版）：

```bash
pnpm tauri build --target x86_64-apple-darwin
# 或同时支持两种架构（通用二进制，体积更大，也更慢）
pnpm tauri build --target universal-apple-darwin
```

不指定 `--target` 时，默认只打当前机器的架构（Apple Silicon 上就是 `aarch64-apple-darwin`）。

## 产物位置

```
src-tauri/target/release/bundle/macos/CC Switch.app
src-tauri/target/release/bundle/dmg/CC Switch_<version>_aarch64.dmg
```

（若指定了 `--target`，路径会带上目标三元组，例如 `target/aarch64-apple-darwin/release/bundle/...`）

## 打开未签名的 App

因为没有走 Apple Developer 签名，直接双击会被 Gatekeeper 拦截提示"无法打开，因为无法验证开发者"。两种绕过方式：

- **Finder 右键 App → 打开**，弹窗里选择"仍要打开"
- 或命令行去除隔离标记：
  ```bash
  xattr -cr "/path/to/CC Switch.app"
  ```

## 完整签名 + 公证流程（对外分发才需要）

仓库的 `.github/workflows/release.yml` 是官方发布用的完整流程，本地要复现需要准备：

| 用途 | 需要的凭据 |
|---|---|
| Tauri Updater 签名 | `TAURI_SIGNING_PRIVATE_KEY`（私钥）+ `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` |
| Apple 代码签名 | `.p12` 证书（`Developer ID Application` 类型）+ 证书密码，导入 Keychain |
| Apple 公证 (notarization) | `APPLE_ID` / `APPLE_PASSWORD`（App 专用密码）/ `APPLE_TEAM_ID` |
| 生成带样式的 DMG | `create-dmg`（`brew install create-dmg`） |

有了以上凭据后，设置好对应环境变量，`createUpdaterArtifacts` 保持 `true`，正常执行 `pnpm tauri build --target universal-apple-darwin` 即可；Tauri 会自动完成签名，公证需要额外用 `xcrun notarytool submit ... --wait` 手动提交（详见 `release.yml` 中 `Notarize macOS DMG` 步骤）。

这套流程需要付费的 Apple Developer Program 账号，个人本地测试不需要走这一套。

## 常见问题

- **打包时报 `pnpm install` 失败 / ignored builds**：执行 `pnpm approve-builds`，选择需要放行的包（一般是 `esbuild`、`msw`）。
- **打包卡在签名步骤报错找不到私钥**：确认 `createUpdaterArtifacts` 是否为 `false`（本地自用打包）或已正确配置 `TAURI_SIGNING_PRIVATE_KEY`（对外分发）。
- **想清理编译缓存重新全量构建**：删除 `src-tauri/target` 目录，注意这会让下次编译从头开始，耗时明显变长。
