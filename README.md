# arch-repo-update

从 [quantulr/archlinux-repo](https://github.com/quantulr/archlinux-repo) 的 GitHub Release manifest 自动同步本地 pacman 仓库：

- 检测新增、升级与同版本重建（sha256 变化）
- 下载并校验 `sha256` / 文件大小
- 以 `archrepo` 用户为每个软件包和仓库数据库生成 GPG 签名
- 运行 `repo-add` 更新数据库
- 清理被替换的旧版本文件
- 删除 manifest 中已不存在的软件包
- 校验并自动补签缺失或失效的签名

默认配置针对本机：仓库目录 `/srv/http/repo/ayaya/os/x86_64`，数据库名 `ayaya`，签名用户 `archrepo`。所有默认值都可以用命令行参数覆盖。

## 工作流程

1. 获取 release 中的 `manifest.json`（默认 URL 见下方参数表）。
2. 读取本地 `<db>.db.tar.zst`，按 `pkgname` 匹配，使用 pacman 的 `vercmp` 比较版本：
   - 本地不存在 → 新增
   - 远端版本更高 → 升级
   - 版本相同但 sha256 不同 → 重建
   - 远端版本更低 → 默认跳过（可用 `--allow-downgrade` 强制）
3. 只把需要同步的包下载到仓库目录内的临时目录，边下载边计算 SHA-256，并核对 manifest 中的 `size` 和 `sha256`。
4. 以 `--sign-user` 指定的用户（默认 `archrepo`）为每个包生成二进制分离签名 `<包名>.sig`，并立即验签。
5. 原子地安装包与签名到仓库目录，所有权改为 `archrepo`。
6. 运行 `repo-add --prevent-downgrade`。成功后删除同一包的旧版本文件和 `.sig`（`--keep-old` 可关闭）。
7. 对本地数据库中存在、但当前 manifest 的目标架构列表中已不存在的包，运行 `repo-remove`，并删除对应的包文件与 `.sig`。此行为默认开启，可用 `--no-prune` 关闭；若 manifest 完全没有目标架构包，必须显式加 `--allow-empty-manifest` 才会删除全部本地包。
8. 重新签名 `.db` 与 `.files` 数据库并维护 `.db.sig` / `.files.sig` 符号链接。每次运行都会校验数据库签名，缺失或失效时自动修复。
9. 对 manifest 中已是最新的包，检查本地 `.sig`；缺失、无效或不是本仓库密钥签发的签名会被自动补签。

单个包下载或签名失败不会阻止其他包同步，但程序最终会以非零状态退出，便于 systemd 或监控发现问题。

> 注意：`repo-add --sign` 无法从文件读取 GPG 口令，因此本程序不使用该选项，而是在 `repo-add` 完成后用 `gpg --pinentry-mode loopback --passphrase-file` 显式签名数据库。效果与 `repo-add --sign` 相同。

## 构建

需要 Arch Linux（提供 `repo-add`、`vercmp`、`pacman`、`gpg`）和 Rust 工具链。

```sh
cargo build --release
sudo install -m 755 target/release/arch-repo-update /usr/bin/arch-repo-update
```

### Arch 软件包

`packaging/` 下提供了 PKGBUILD，直接使用当前检出构建：

```sh
cd packaging
makepkg -f --nodeps          # 本机 cargo/rustc 由 rustup 提供
sudo pacman -U arch-repo-update-0.1.0-4-x86_64.pkg.tar.zst
```

如果使用仓库中的 `cargo` 与 `rust` 包，可省略 `--nodeps`，或直接 `makepkg -si`。

软件包会安装：

- `/usr/bin/arch-repo-update`
- `/usr/lib/systemd/system/arch-repo-update.{service,timer}`
- `/usr/lib/sysusers.d/arch-repo-update.conf`（创建 `archrepo` 用户）
- `/usr/lib/tmpfiles.d/arch-repo-update.conf`（创建 `/var/lib/archrepo`）
- 文档与许可证

PKGBUILD 使用 `source=()` 从当前检出构建，适合本地打包；发布到公共仓库后建议改为固定 commit 或源码 tarball 的 `source`。

## 首次配置

### 1. GPG 口令文件

`archrepo` 的私钥受口令保护。创建一个仅 `archrepo` 可读的口令文件（第一行即口令）：

```sh
sudo install -d -o archrepo -g archrepo -m 700 /etc/arch-repo-update
sudo -u archrepo bash -c 'IFS= read -rs -p "GPG 口令: " p; printf "%s\n" "$p" >/etc/arch-repo-update/passphrase; chmod 600 /etc/arch-repo-update/passphrase'
```

### 2. 修正历史文件所有权（可选）

如果仓库中已有 root 所有的文件：

```sh
ls -la /srv/http/repo/ayaya/os/x86_64   # 先确认目录
sudo arch-repo-update --fix-ownership
```

`--fix-ownership` 会递归地把仓库目录内所有文件（不跟随符号链接）的所有者改为签名用户，需要 root。

### 3. 试运行

```sh
sudo arch-repo-update --dry-run --verbose
sudo arch-repo-update --passphrase-file /etc/arch-repo-update/passphrase
```

## systemd 定时同步

仓库文件与 GPG 主目录都归 `archrepo` 所有，因此服务主体以 `archrepo` 用户运行，不需要 root（仅用 `ExecStartPre` 以 root 创建 GPG 运行时目录）：

```sh
sudo install -m 644 contrib/arch-repo-update.service /etc/systemd/system/
sudo install -m 644 contrib/arch-repo-update.timer /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now arch-repo-update.timer
```

通过软件包安装时，两个单元已经在 `/usr/lib/systemd/system/`，只需执行 `daemon-reload` 和 `enable` 两步。

定时器默认每 30 分钟检查一次，并带最多 2 分钟随机延迟。上游通常每天 00:00 UTC 构建，也可以手动触发，因此高频检查能更快发现更新。如需调整频率，修改 `[Timer]` 中的 `OnCalendar`（例如每 15 分钟：`*-*-* *:00/15:00`），然后执行：

```sh
sudo systemctl daemon-reload
sudo systemctl restart arch-repo-update.timer
```

查看下次运行时间与手动触发：

```sh
systemctl list-timers arch-repo-update.timer
sudo systemctl start arch-repo-update.service
journalctl -u arch-repo-update.service -n 100
```

## 参数

| 参数 | 默认值 | 说明 |
| --- | --- | --- |
| `--repo-dir <PATH>` | `/srv/http/repo/ayaya/os/x86_64` | 仓库目录 |
| `--db-name <NAME>` | `ayaya` | 数据库名，生成 `<NAME>.db.tar.zst` |
| `--db-path <PATH>` | 由上面两项推导 | 直接指定数据库路径 |
| `--manifest <URL\|PATH>` | `https://github.com/quantulr/archlinux-repo/releases/download/packages/manifest.json` | manifest 来源，支持 HTTP(S)、`file://` 和本地路径 |
| `--arch <ARCH>` | 当前机器架构（如 `x86_64`） | 只同步该架构的包 |
| `--sign-user <USER>` | `archrepo` | 运行 gpg 与 repo-add 的用户 |
| `--gpg-home <PATH>` | `<sign-user> 的 ~/.gnupg` | GPG 主目录 |
| `--passphrase-file <PATH>` | 无 | GPG 口令文件；私钥有口令时必须提供 |
| `--key <KEY>` | 默认私钥 | 指定签名密钥（指纹、key ID 或 UID） |
| `--allow-downgrade` | 关闭 | 允许同步比本地版本更低的远端版本 |
| `--keep-old` | 关闭 | 保留被替换的旧版本软件包文件 |
| `--no-prune` | 关闭 | 不删除 manifest 中已不存在的本地软件包 |
| `--allow-empty-manifest` | 关闭 | manifest 无目标架构包时仍允许删除全部本地软件包 |
| `--fix-ownership` | 关闭 | 递归修正仓库文件所有者为签名用户（需要 root） |
| `--dry-run` | 关闭 | 只显示计划，不下载、不签名、不修改仓库 |
| `--no-sign` | 关闭 | 不签名，仅下载并更新数据库（调试用） |
| `--timeout <SECS>` | `600` | HTTP 超时 |
| `-v, --verbose` | 关闭 | 输出详细日志 |

运行身份：需要以 root 或 `--sign-user` 指定的用户运行。以 root 运行时，gpg、`repo-add` 和 `repo-remove` 仍会切换到签名用户执行，下载的文件会被 `chown` 给该用户；以 `archrepo` 运行时则全部直接执行。

## 故障排查

### `gpg: database_open ... waiting for lock`

通常是 `archrepo` 的 `keyboxd` 异常退出或残留进程占用了公钥数据库锁。找到并终止残留进程：

```sh
ps -eo pid,user,etime,args | grep keyboxd
sudo kill <PID>
```

也可以尝试：

```sh
sudo -u archrepo gpgconf --kill all
```

GnuPG 2.4 优先把 agent/keyboxd 套接字放在 `/run/user/<uid>/gnupg`，该目录不存在时退回 `~/.gnupg`。两种位置各启动一个 keyboxd 时会争用同一个公钥数据库锁，表现为 `waiting for lock ... Connection timed out`。本程序会显式使用已存在且属于签名用户的 `/run/user/<uid>`；systemd 服务也会通过 `ExecStartPre` 创建 `/run/user/<uid>` 与 `/run/user/<uid>/gnupg`。手动准备：

```sh
sudo install -d -o archrepo -g archrepo -m 700 /run/user/$(id -u archrepo) /run/user/$(id -u archrepo)/gnupg
```

如果 `gpgconf --kill all` 无法清理残留进程（它只会联系当前套接字对应的 agent），用上面的 `ps` + `kill` 按 PID 终止。

### `gpg: failed to start gpg-agent` / `can't connect to the gpg-agent`

通常是 `/run/user/<uid>/gnupg` 不存在或不可写，或者 systemd 单元启用了 `ProtectHome=read-only`。`/run/user` 位于 `/run` tmpfs 上，`ProtectHome=read-only` 会把它变成只读，导致 gpg-agent 无法创建套接字。确保目录存在并归签名用户所有：

```sh
sudo install -d -o archrepo -g archrepo -m 700 /run/user/$(id -u archrepo)/gnupg
```

本项目的 systemd 单元已通过 `ExecStartPre` 创建该目录，并且不使用 `ProtectHome=read-only`。

### `GPG 签名失败` / `No passphrase`

确认 `--passphrase-file` 指向的文件存在、第一行是正确口令，并且 `archrepo` 可读：

```sh
sudo -u archrepo test -r /etc/arch-repo-update/passphrase && echo readable
```

### 用户对仓库目录没有写权限

```sh
sudo arch-repo-update --fix-ownership
```

### 签名无效或不属于仓库密钥

程序会自动用当前密钥重新签名数据库和已存在的包。若刚刚更换过密钥，先确认 `--key` 或 `--gpg-home` 指向正确密钥，然后运行一次同步即可补签。

## 安全性说明

- `manifest.json` 是本程序的信任根。它通过 HTTPS 从固定的 GitHub Release URL 获取；包内容再用 manifest 中的 `sha256` 校验。
- 程序会以 `archrepo` 的私钥签名下载的内容。请确保 manifest 来源（GitHub 仓库/Release）本身可信。
- 口令文件应保持 `0600` 且仅签名用户可读；程序发现权限过宽时会给出警告。
- `--no-sign` 仅用于调试，会使数据库在本次运行后失去签名。

## 许可证

MIT
