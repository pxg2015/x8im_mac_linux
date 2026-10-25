# X8.IM 桌面客户端 · macOS / Linux 构建（开源）

本仓库仅用于 **X8.IM 桌面客户端** 的 **macOS / Linux** 平台 CI 构建。

- 产品主页：https://im.x8fde.com/app/
- 完整项目（含 Windows 客户端、服务端、前端）为独立私有仓库；本仓只承载跨平台开源构建部分。

## 这是什么

GitHub Actions 在本仓自动编译并产出 **未签名** 的安装包：

| 平台 | 产物 |
|---|---|
| macOS（Apple Silicon + Intel） | `X8.IM-apple-silicon.dmg` / `X8.IM-intel.dmg` |
| Linux（x86_64 + aarch64） | `.deb` / `.rpm` / `.AppImage` |

产物通过 Actions **artifact** 分发；**签名与发布由项目维护者在本地完成**，私钥不在此仓库。

## 目录结构

```
.
├── .github/workflows/
│   ├── build-macos.yml      # macOS 双架构构建
│   └── build-linux.yml      # Linux 双架构构建
└── desktop/
    ├── package.json         # npm ci → npx tauri build
    ├── package-lock.json
    └── src-tauri/           # Rust 源码 + Tauri 配置（不含 bin/ 等运行时资源）
```

主窗口加载远端 `https://im.x8fde.com/app/`，仓库内的 `dist/` 仅为满足 Tauri 打包要求生成的占位页，不参与实际运行。

## 本地构建

```bash
cd desktop
npm ci
npx tauri build --target aarch64-apple-darwin   # macOS (Apple Silicon)
npx tauri build --target x86_64-apple-darwin    # macOS (Intel)
npx tauri build --bundles deb,rpm               # Linux
```

## 构建所需的 Secrets（远程协助用）

远程协助功能依赖自建 RustDesk 服务器。其**地址与连接公钥**由构建期通过环境变量注入，
**不写死在源码**（保持本仓基础设施无关）。请在仓库 `Settings → Secrets and variables → Actions`
中配置以下两个仓库级 Secret（值由维护者持有；公钥本身可公开，但服务器地址属内部基础设施，勿外泄）：

| Secret 名 | 含义 |
|---|---|
| `RD_SERVER` | 自建 RustDesk 服务器公网地址（hbbs/hbbr） |
| `RD_SERVER_KEY` | 客户端连接用的 RustDesk 公钥 |

> 未配置时构建仍成功，但远程协助静默不可用（不影响其它功能）。
> 本地构建请在执行 `npx tauri build` 前 `export RD_SERVER=... RD_SERVER_KEY=...`（私有仓的 `_build.sh` 亦同）。

## 安全说明

- 本仓库 **不包含任何私钥、密码、服务器凭据或用户数据**。
- 远程协助所需的自建 RustDesk 服务器地址/公钥经上述 Secrets 注入，**源码中无任何明文**。
- Tauri 更新签名私钥仅在维护者本地机器，通过本地脚本完成签名与分发。
- `tauri.conf.json` 中的 `plugins.updater.pubkey` 为公钥（本就该随客户端公开），`endpoints` 为公开域名。
