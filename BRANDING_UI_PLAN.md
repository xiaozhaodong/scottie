# Scottie 品牌改造方案

最后更新：2026-08-23

## 1. 当前基线

- 上游仓库：https://github.com/l0ng-ai/tty7
- 个人 fork：https://github.com/xiaozhaodong/scottie
- 上游与 fork 的 main 已同步到：74bb98697d8621b7b243d2d9aa19edbeab26c29e
- 本地 clone：/Users/xiaozhaodong/VsCodeProjects/tty7
- 当前本地分支：branding/scottie

这份方案只覆盖**对外品牌**。不改核心终端协议、远程连接、Agent 生命周期和数据格式。

## 2. 定名：Scottie

`tty7` 这个名字过于潦草，改为 **Scottie**。

### 2.1 可用性核查结果（2026-08-23 实测）

```
crates.io/scottie   FREE
npm/scottie         FREE
GitHub              无有分量的同名项目（最高 34★，无关的 Arduino SSTV 编码器）
```

对比被排除的候选：

| 候选 | 排除原因 |
|---|---|
| Relay | Meta 的 GraphQL 客户端，开发者圈撞名严重 |
| Berth | crates.io 上已是 "MCP servers 的 runtime & package manager"，AI 工具链正面相邻 |
| Perch | crates.io 上已是 "a beautiful terminal social client"，同属终端工具 |
| Scotty | crates.io 有 8281 下载的终端 dir switcher；spatie/scotty 是 SSH task runner（286★）；scotty-web/scotty 是 Haskell 框架（1776★） |
| Cairn | 可用，但 Scottie 记忆点更强 |

### 2.2 选它的理由

- **Scottie = 苏格兰㹴犬**。终端圈已有 kitty（猫），Scottie 是狗，天然成对，圈内人一看会心
- 图标是方头方脑的㹴犬剪影，单色 16×16 极好辨认
- 含 `tt`，`Scot-tie` 读出来就是 scotty，保留 `-tty` 谱系的暗号，但不挤进 kitty/Ghostty 的 SEO 战场
- 这个拼法明确指向狗/人名，规避 Star Trek 角色的商标指涉

> ⚠️ **不要在 README、发布说明或任何文案里用 "Beam me up" 一类的 Star Trek 梗。**
> 单独叫 Scottie 是常见人名，没问题；一旦挂上 Star Trek 隐喻，就从「碰巧同名」变成「明确指涉」Paramount 的角色。

### 2.3 品牌参数

```
产品名        Scottie
Bundle ID     ai.scottie.app
仓库          xiaozhaodong/scottie（fork 改名）
Tagline       A terminal workbench that fetches, waits, and never lets go.
中文           会话不散的终端工作台
```

㹴犬的性格是叼住不放、耐心守着 —— 正好对应 daemon 持有会话、`wait --until free`。这条语义线不用硬编。

## 3. 改动范围实测

全仓库 `tty7` 相关引用：**241 个文件、约 1600 处**（已排除 `target/` 和 `Cargo.lock`）。

按层分布和处置：

| 层 | 量 | 处置 |
|---|---|---|
| 显示层（i18n、菜单、About、通知） | ~228 处 | 改 |
| 打包层（Info.plist、安装器、图标） | ~157 处 | 改 macOS 部分 |
| 文档（README、docs/） | ~302 处 | 改 |
| **运行时层**（bin 名、env、socket、配置目录、远程 server） | **~900 处** | **不动** |

### 3.1 运行时层为什么不动

grep 出三个硬伤：

1. **远程 server 路径写死了名字** —— `src/ui/remote_connect.rs:844` 是 `~/.local/share/tty7/bin/tty7-server-0.9.1`。改名后新客户端会去找 `scottie-server`，所有远程机器上装的 `tty7-server` 全部失联，每台都要重装
2. **配置目录 / socket 有 158 处路径引用** —— 改名等于旧配置、旧会话、正在跑的 daemon 全部失联，新旧版本会各起一个互不认识的 daemon
3. **`skills/tty7/SKILL.md` 有 52 处** —— 这是给 agent 看的说明书，告诉它敲 `tty7 run`；改名后已学会调 `tty7` 的 agent 会话全部失效

### 3.2 保持不变的清单

```
Cargo package / bin      tty7、tty7-app、tty7-updater、tty7-cli、tty7-core
远程                     tty7-server、远程目录、启动参数
环境变量                  TTY7_*
路径                     配置目录、socket、临时目录
协议                     所有协议字段
skills/tty7/             agent 技能定义
```

等品牌版本稳定后再单独评估「内部彻底重命名」，不作为本轮目标。

## 4. 设计原则

1. **显示层改名，运行时不改名** —— 降低升级、配置迁移和远程兼容风险
2. **每一步可独立回滚** —— 不把品牌改动和功能改动混在同一个提交
3. **改动集中、易 rebase** —— 要长期跟随上游，品牌 commit 要小且集中
4. **品牌资源可追溯** —— 图标保留源文件与许可证记录

> **不再采用**：原方案设想的 `src/core/branding.rs` 品牌常量模块。实测发现 i18n 里全是硬编码在句子中间的字面量（如 `"Choose the language used for the tty7 interface."`），常量替换不了；Info.plist 是 shell 脚本，也用不到 Rust 常量。真正需要统一的只有更新源地址一个值，写在 `update.rs` 即可。建这个模块属于过度设计。

## 5. 修改清单

### 5.1 第一批：本地可用（不碰 Rust 代码）

```
① git remote add upstream https://github.com/l0ng-ai/tty7.git
② cargo build --release --bin tty7-app                    基线验证
③ .github/scripts/bundle-macos.sh 改 4 行：
     :30  APP="dist/tty7.app"                → dist/Scottie.app
     :65  CFBundleName        tty7           → Scottie
     :66  CFBundleDisplayName tty7           → Scottie
     :67  CFBundleIdentifier  com.github.tty7 → ai.scottie.app
④ 新增 scripts/brand-build.sh：build + bundle + 装到 /Applications，一条命令
```

改完 Dock、Finder、⌘Tab、菜单栏左上角全部显示 Scottie。

**注意** `bundle-macos.sh` 第 34/41/50 行的 `tty7-app`、`tty7`、`tty7-updater` 是桶内可执行文件名，`CFBundleExecutable` 指着它们 —— 不要动，动了要连带改 GUI 解析 CLI 路径的逻辑。

### 5.2 第二批：发行前必做

因为最终会发到 GitHub 给别人用，下面几项从「可选」变成「必做」：

| 项 | 文件 | 不做的后果 |
|---|---|---|
| **更新源改到自己的 fork** | `src/core/update.rs` | ⚠️ 最要命：别人装了 Scottie，更新器从 `l0ng-ai/tty7` 拉官方包，**把 Scottie 覆盖回 tty7** |
| Bundle ID | `bundle-macos.sh` | macOS LaunchServices 里两个 app 抢同一个 `com.github.tty7` 注册，「用…打开」错乱 |
| i18n 文案替换 | `src/ui/i18n/{en,zh,ja}.rs` | 叫 Scottie 的应用，设置页写「tty7 的自动补全菜单」，用户困惑 |
| 换图标 | `assets/tty7.icns`、`app-icon.png`、`favicon.ico`、`app-icon.svg` | 顶着上游图标发行造成混淆 |
| README 改名 + fork 声明 | `README.md`、`README.zh-CN.md` | Apache-2.0 要求保留版权声明并标注修改 |
| 「勿与官方 tty7 同装」警告 | README | 见 7.2 |

i18n 那 228 处用 `sed` 批量替换即可。代价是上游改文案时 rebase 会冲突，解法是重跑一遍 sed。

### 5.3 暂不做

- Windows / Linux 打包脚本（`windows-installer.iss`、`bundle-linux.sh`、`bundle-appimage.sh`）—— 等真要发这两个平台再补
- `docs/` 下对外产品文案、站点标题和仓库链接已切换为 Scottie；CLI、路径和协议示例保留 tty7（本轮已处理）
- 侧边栏和主题视觉改造 —— **已取消**，见 6

## 6. 已取消：UI 视觉改造

原方案 2.2 / 4.3 / Phase 2 / Phase 3 规划的主题 token 统一和侧边栏精修**全部取消**。

原因：渲染观感问题的实际根因是字体平滑，已通过对 tty7 单独设置 `AppleFontSmoothing=0` 解决，当前渲染效果与 Otty 基本一致。侧边栏的信息架构本身没有问题，不需要重排。

## 7. 风险和边界

### 7.1 更新器风险

Bundle ID 变化会让旧版本无法自动识别新版本。本轮明确**不支持 tty7 → Scottie 自动迁移**：旧 tty7 安装需要从发布页手动安装 Scottie；Scottie 自身只接受 `Scottie.app` 和 `scottie-*` macOS 更新包。这样可以避免旧 Bundle ID、旧 app 路径和新签名要求混用。Scottie 内部的后续更新仍需在测试渠道验证升级和回滚，不能只看静态配置。

### 7.2 与官方 tty7 并行运行

配置目录、socket 和临时目录仍然是 `tty7`，所以**官方 tty7 和 Scottie 会共享同一个后台 daemon 和运行状态**，同时运行会打架。

处置：把 Scottie 定位为 tty7 的替代发行版，README 里明确写「请勿与官方 tty7 同时安装/运行」。若以后必须支持并行，另开任务设计配置/运行时隔离。

### 7.3 许可证

上游是 Apache-2.0，可以 fork 并修改，但必须：

- 保留 LICENSE、版权声明和必要的 NOTICE
- 在 README 中标注这是 `l0ng-ai/tty7` 的 fork 并说明改动
- 不复制 Otty（闭源）的代码、图标或未授权资源

## 8. 分支建议

当前 main 只用于跟随上游同步。开发时：

- `branding/scottie` —— 显示层品牌、图标、打包元数据、更新地址、README 与 docs

每个分支保持可构建、可回滚。品牌 commit 尽量小且集中，方便长期 rebase 跟随上游。

## 9. 验收清单

第一批：

- [ ] Dock、Finder、⌘Tab、菜单栏显示 Scottie
- [ ] `cargo build --release --bin tty7-app` 通过
- [ ] `scripts/brand-build.sh` 一条命令完成构建安装
- [ ] 旧配置、远程连接和 Agent 启动流程未被破坏

第二批（发行前）：

- [ ] 更新检查指向自有发行源，升级/回滚已实际验证
- [ ] 应用菜单、About、通知、设置页全部显示 Scottie
- [ ] macOS 安装包的显示名和图标正确
- [ ] README 已改名、声明 fork、保留 NOTICE、写明勿与官方 tty7 同装
- [ ] `cargo fmt --check` 通过
- [ ] `cargo test --workspace` 通过

## 10. 待定

- **图标**：尚未产出。方向是方头方脑的㹴犬剪影，需要 icns / png / ico / svg 四种格式。第一批先沿用现有 tty7 图标，不阻塞改名
- **GitHub 仓库改名**：`xiaozhaodong/tty7` → `xiaozhaodong/scottie`，已与更新源地址同步
- **域名**：未查 scottie.dev / scottie.sh / scottie.app 的占用情况，非必需
