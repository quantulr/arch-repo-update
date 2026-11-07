# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## 项目概述

`arch-repo-update` 是一个 Rust 单二进制工具：从 `quantulr/archlinux-repo` Release 中的 `manifest.json` 同步本地 pacman 仓库——检测新增/升级/重建，下载并校验 `sha256` 与大小，为软件包和数据库生成 GPG 分离签名，运行 `repo-add`，清理旧版本以及上游已移除的包。

上游构建定义（`packages/*.toml`）在另一个仓库 `quantulr/archlinux-repo`，本仓库只消费其 Release 产物与 `manifest.json`。

默认目标是本机仓库 `/srv/http/repo/ayaya/os/x86_64`（数据库 `ayaya`），签名用户 `archrepo`。用户可见输出、README 和提交信息使用中文；代码注释与内部错误上下文使用英文。

## 常用命令

```sh
cargo build --release
cargo test                        # 单元测试；依赖 pacman 提供的 vercmp
cargo test classifies_updates     # 按名称运行单个测试
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

真实同步需要 root 或 `archrepo` 用户、`pacman`/`gnupg`、签名私钥与口令文件：

```sh
sudo arch-repo-update --dry-run --verbose
sudo arch-repo-update --passphrase-file /etc/arch-repo-update/passphrase
```

打包时 PKGBUILD 直接使用当前检出（`source=()`，不下载源码）：

```sh
cd packaging
makepkg -f --nodeps               # 本机 cargo/rustc 来自 rustup
sudo pacman -U arch-repo-update-0.1.0-<pkgrel>-x86_64.pkg.tar.zst
```

修改代码、`contrib/` 单元或 PKGBUILD 内容后，需递增 `packaging/PKGBUILD` 的 `pkgrel`。

## 架构

数据流（`src/main.rs` 的 `run`）：

1. `manifest.rs` 获取并反序列化 `manifest.json`。manifest 是信任根：从固定 HTTPS URL 获取，包内容再用其中的 `sha256`/`size` 校验。
2. `sync::build_plan` 用 `util::vercmp` 比较 manifest 与本地数据库，得出 `New / Update / Rebuild / UpToDate / Downgrade`，并计算已从 manifest 移除的本地包。
3. `sync::execute` 在仓库目录内的 `.arch-repo-update.staging.<pid>/` 下载（流式 SHA-256、大小校验、3 次重试），先签名再原子安装，随后一次性 `repo-add`；清理被替换的旧文件；按需 `repo-remove` 并删除文件；最后签名数据库，并为已是最新的包补签缺失/失效的签名。

模块职责：

- `gpg.rs` — `Runner` 是所有外部命令（`gpg`、`repo-add`、`repo-remove`、`test`）的唯一入口。以 root 运行时通过 `runuser -u archrepo --` 切换用户，并固定 `cwd=/`、`HOME`、`GNUPGHOME`、`PATH`、`LC_ALL=C`；当 `/run/user/<uid>` 存在且属于该用户时固定 `XDG_RUNTIME_DIR`，避免两套 gpg-agent/keyboxd 争用同一密钥环。`Signer` 列出私钥指纹（主钥和子钥），以 `--pinentry-mode loopback --passphrase-file` 签名，并按 `VALIDSIG` 指纹验签。
- `local_repo.rs` — 读取 `repo-add` 数据库（解压 `.zst`/`.gz` tar，只解析 `desc` 条目）、`repo-add`/`repo-remove`、安装/删除包文件、签名数据库并维护 `<db>.sig -> <db>.tar.*.sig` 符号链接（符号链接用 `lchown`）。
- `util.rs` — 日志、`fetch_manifest`（支持 http(s)、`file://`、本地路径）、`vercmp` 调用，以及安全边界 `sanitize_filename`/`sanitize_pkgname`。manifest 与数据库内容都是不可信输入，进入路径或 argv 前必须经过它们。
- `sync.rs` — 计划与执行。单个包失败会累计，最后以非零状态退出，但不会阻止其他包；数据库签名失败则立即中止。

## 关键约束

- 不使用 `repo-add --sign`：它无法从文件读取口令。数据库总是在 `repo-add` 之后由 `local_repo::sign_databases` 显式签名，效果等价。
- 所有写入必须原子：下载先写 `.part`，签名先写 `.sig.tmp.<pid>`，校验通过后再 `rename`。
- `manifest.size` 同时作为下载上限和精确大小校验；字段缺失时用 8 GiB 兜底。
- 数据库路径必须包含 `.db.tar.`（`local_repo::validate_db_path`），`.files.tar.*` 路径由它推导。
- 执行前对仓库目录加排他 `flock`；正因持有该锁，`StagingDir::create` 才能安全清理上次崩溃遗留的 staging 目录。
- prune 默认开启：manifest 中不存在的本地包会被 `repo-remove` 并删除文件；若目标架构的 manifest 为空，则拒绝删除全部本地包，除非加 `--allow-empty-manifest`。
- 运行身份必须是 root 或 `--sign-user` 指定的用户（默认 `archrepo`）；其他用户会被拒绝。
- systemd 单元以 `archrepo` 运行，`ExecStartPre=+`（root）创建 `/run/user/<uid>` 与 `/run/user/<uid>/gnupg`。不要加 `ProtectHome=read-only`——`/run/user` 位于 `/run` tmpfs，会被一并只读化，导致 gpg-agent 无法创建套接字。`ProtectSystem=full` 保持 `/srv` 与 `/var/lib/archrepo` 可写。
- 单元与配置模板在 `contrib/`，打包和安装脚本在 `packaging/`；随包安装的文件清单见 `packaging/PKGBUILD` 的 `package()`。
