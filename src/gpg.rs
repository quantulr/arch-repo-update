use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::util;

const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// A local account used to run GPG and `repo-add`, so that every object in the
/// repository is owned and signed by the repository user.
#[derive(Debug, Clone)]
pub struct UserInfo {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub home: PathBuf,
}

impl UserInfo {
    pub fn lookup(name: &str) -> Result<Self> {
        let passwd = fs::read_to_string("/etc/passwd").context("读取 /etc/passwd 失败")?;
        for line in passwd.lines() {
            let fields: Vec<&str> = line.split(':').collect();
            if fields.len() >= 7 && fields[0] == name {
                return Ok(Self {
                    name: fields[0].to_string(),
                    uid: fields[2]
                        .parse()
                        .with_context(|| format!("解析用户 {name} 的 uid 失败"))?,
                    gid: fields[3]
                        .parse()
                        .with_context(|| format!("解析用户 {name} 的 gid 失败"))?,
                    home: PathBuf::from(fields[5]),
                });
            }
        }
        bail!("系统中不存在用户 {name}（请先创建，或通过 --sign-user 指定）");
    }

    pub fn gpg_home(&self) -> PathBuf {
        self.home.join(".gnupg")
    }
}

pub fn current_euid() -> u32 {
    // SAFETY: geteuid 始终成功且没有副作用。
    unsafe { libc::geteuid() }
}

#[derive(Debug, Clone)]
pub struct Runner {
    pub user: UserInfo,
    pub gpg_home: PathBuf,
}

impl Runner {
    pub fn new(user_name: &str, gpg_home: Option<PathBuf>) -> Result<Self> {
        let user = UserInfo::lookup(user_name)?;
        let gpg_home = match gpg_home {
            Some(path) => fs::canonicalize(&path)
                .with_context(|| format!("GPG 主目录不存在: {}", path.display()))?,
            None => user.gpg_home(),
        };
        Ok(Self { user, gpg_home })
    }

    pub fn can_switch_to_user(&self) -> bool {
        let euid = current_euid();
        euid == 0 || euid == self.user.uid
    }

    pub fn command(&self, program: &str) -> Result<Command> {
        let euid = current_euid();
        let mut command = if euid == self.user.uid {
            Command::new(program)
        } else if euid == 0 {
            let mut command = Command::new("runuser");
            command
                .arg("-u")
                .arg(&self.user.name)
                .arg("--")
                .arg(program);
            command
        } else {
            bail!(
                "需要以 root 或用户 {} 运行（当前 uid 为 {}）",
                self.user.name,
                euid
            );
        };

        command
            // The target user may not be able to traverse the caller's current
            // directory. All paths passed to child processes are absolute.
            .current_dir("/")
            .env("HOME", &self.user.home)
            .env("USER", &self.user.name)
            .env("LOGNAME", &self.user.name)
            .env("GNUPGHOME", &self.gpg_home)
            .env("PATH", DEFAULT_PATH)
            .env("LC_ALL", "C");

        // GnuPG 2.4 uses $XDG_RUNTIME_DIR/gnupg for its sockets when that
        // directory exists and falls back to the GNUPGHOME otherwise. Pin it
        // explicitly so a systemd service and a manual root run use the same
        // agent and keyboxd instead of creating two competing instances.
        let runtime_dir = PathBuf::from(format!("/run/user/{}", self.user.uid));
        if let Ok(metadata) = fs::metadata(&runtime_dir)
            && metadata.is_dir()
            && metadata.uid() == self.user.uid
        {
            command.env("XDG_RUNTIME_DIR", &runtime_dir);
        }

        Ok(command)
    }

    /// Check that the repository user can write to and traverse `dir`.
    pub fn check_writable(&self, dir: &Path) -> Result<()> {
        let status = self
            .command("test")?
            .arg("-w")
            .arg(dir)
            .arg("-a")
            .arg("-x")
            .arg(dir)
            .status()
            .context("检查目录权限失败")?;
        if !status.success() {
            bail!(
                "用户 {} 对目录 {} 没有写权限，请先修复所有者（例如 --fix-ownership）",
                self.user.name,
                dir.display()
            );
        }
        Ok(())
    }

    /// Make a staging directory writable by the repository user without
    /// changing the ownership of the repository itself.
    pub fn prepare_staging(&self, dir: &Path) -> Result<()> {
        self.chown(dir)?;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o755))
            .with_context(|| format!("设置 {} 权限失败", dir.display()))?;
        self.check_writable(dir)
    }

    /// Change ownership to the repository user when running as root. Files
    /// created earlier by the repository user itself need no change.
    pub fn chown(&self, path: &Path) -> Result<()> {
        if current_euid() != 0 {
            return Ok(());
        }
        std::os::unix::fs::chown(path, Some(self.user.uid), Some(self.user.gid))
            .with_context(|| format!("将 {} 的所有者改为 {} 失败", path.display(), self.user.name))
    }

    /// Like [`Runner::chown`] but does not follow symlinks.
    pub fn chown_symlink(&self, path: &Path) -> Result<()> {
        if current_euid() != 0 {
            return Ok(());
        }
        use std::os::unix::ffi::OsStrExt;

        let path_c = std::ffi::CString::new(path.as_os_str().as_bytes())
            .with_context(|| format!("路径包含 NUL 字符: {}", path.display()))?;
        // SAFETY: path_c 是有效的以 NUL 结尾的 C 字符串。
        let result = unsafe { libc::lchown(path_c.as_ptr(), self.user.uid, self.user.gid) };
        if result != 0 {
            return Err(std::io::Error::last_os_error()).with_context(|| {
                format!("将 {} 的所有者改为 {} 失败", path.display(), self.user.name)
            });
        }
        Ok(())
    }

    /// Recursively fix ownership of an existing repository. Only meaningful
    /// when running as root.
    pub fn fix_ownership(&self, root: &Path) -> Result<()> {
        if current_euid() != 0 {
            bail!("--fix-ownership 需要 root 权限");
        }
        fn walk(runner: &Runner, path: &Path) -> Result<()> {
            let metadata = fs::symlink_metadata(path)
                .with_context(|| format!("读取 {} 的元数据失败", path.display()))?;
            if metadata.file_type().is_symlink() {
                runner.chown_symlink(path)?;
                return Ok(());
            }
            runner.chown(path)?;
            if metadata.is_dir() {
                for entry in fs::read_dir(path)
                    .with_context(|| format!("读取目录 {} 失败", path.display()))?
                {
                    let entry = entry?;
                    walk(runner, &entry.path())?;
                }
            }
            Ok(())
        }
        walk(self, root)?;
        util::info(format!(
            "已将 {} 下所有文件的所有者修正为 {}",
            root.display(),
            self.user.name
        ));
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct Signer {
    pub runner: Runner,
    /// Primary and subkey fingerprints accepted as this repository's signer.
    pub fingerprints: Vec<String>,
    pub passphrase_file: Option<PathBuf>,
}

impl Signer {
    pub fn new(
        runner: Runner,
        key: Option<&str>,
        passphrase_file: Option<PathBuf>,
    ) -> Result<Self> {
        let passphrase_file = match passphrase_file {
            Some(path) => {
                let path = fs::canonicalize(&path)
                    .with_context(|| format!("找不到 GPG 口令文件: {}", path.display()))?;
                check_passphrase_file(&runner, &path)?;
                Some(path)
            }
            None => None,
        };
        let fingerprints = list_secret_fingerprints(&runner, key)?;
        util::debug(format!(
            "签名用户 {}，密钥 {}，GPG 目录 {}",
            runner.user.name,
            fingerprints[0],
            runner.gpg_home.display()
        ));
        Ok(Self {
            runner,
            fingerprints,
            passphrase_file,
        })
    }

    pub fn primary_fingerprint(&self) -> &str {
        &self.fingerprints[0]
    }

    /// Create a binary detached signature next to `file`.
    ///
    /// The signature is written to a temporary path and renamed into place so
    /// a concurrently served repository never sees a half-written signature.
    pub fn sign(&self, file: &Path) -> Result<PathBuf> {
        let signature = util::sig_path(file);
        let temporary = {
            let mut name = signature.as_os_str().to_os_string();
            name.push(format!(".tmp.{}", std::process::id()));
            PathBuf::from(name)
        };
        let _ = fs::remove_file(&temporary);

        let mut command = self.runner.command("gpg")?;
        command.args([
            "--batch",
            "--yes",
            "--no-tty",
            "--no-armor",
            "--detach-sign",
            "--local-user",
            self.primary_fingerprint(),
            "--output",
        ]);
        command.arg(&temporary);
        if let Some(passphrase_file) = &self.passphrase_file {
            command
                .args(["--pinentry-mode", "loopback", "--passphrase-file"])
                .arg(passphrase_file);
        }
        command.arg(file);

        let result = (|| -> Result<()> {
            let output = command.output().context("执行 gpg 签名失败")?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let hint = if self.passphrase_file.is_none() {
                    "（如果私钥受口令保护，请通过 --passphrase-file 提供口令文件）"
                } else {
                    ""
                };
                bail!("GPG 签名失败{hint}: {}", stderr.trim());
            }

            let metadata = fs::metadata(&temporary)
                .with_context(|| format!("签名文件未生成: {}", temporary.display()))?;
            if !metadata.is_file() || metadata.len() == 0 {
                bail!("GPG 生成了无效的签名文件: {}", temporary.display());
            }
            // repo-add 拒绝大于 16 KiB 的软件包签名。
            if metadata.len() > 16 * 1024 {
                bail!("GPG 签名文件过大: {}", temporary.display());
            }
            fs::set_permissions(&temporary, fs::Permissions::from_mode(0o644))
                .with_context(|| format!("设置 {} 权限失败", temporary.display()))?;
            self.runner.chown(&temporary)?;
            fs::rename(&temporary, &signature)
                .with_context(|| format!("安装签名 {} 失败", signature.display()))?;
            Ok(())
        })();

        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result?;
        Ok(signature)
    }

    /// Return true only when `signature` is a valid signature of `file` made
    /// by one of this repository's keys.
    pub fn verify(&self, signature: &Path, file: &Path) -> Result<bool> {
        let mut command = self.runner.command("gpg")?;
        command
            .args(["--batch", "--no-tty", "--status-fd", "1", "--verify"])
            .arg(signature)
            .arg(file);
        let output = command.output().context("执行 gpg 验证失败")?;

        let mut valid_fingerprint: Option<String> = None;
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            if let Some(rest) = line.strip_prefix("[GNUPG:] VALIDSIG ") {
                valid_fingerprint = rest.split_whitespace().next().map(str::to_string);
                break;
            }
        }

        let matches_expected = valid_fingerprint.as_deref().is_some_and(|fingerprint| {
            self.fingerprints
                .iter()
                .any(|expected| expected.eq_ignore_ascii_case(fingerprint))
        });

        let ok = output.status.success() && matches_expected;
        if !ok {
            util::debug(format!(
                "签名验证未通过: {}（state={}，指纹={:?}）",
                file.display(),
                output.status,
                valid_fingerprint
            ));
        }
        Ok(ok)
    }
}

fn check_passphrase_file(runner: &Runner, path: &Path) -> Result<()> {
    let metadata =
        fs::metadata(path).with_context(|| format!("找不到 GPG 口令文件: {}", path.display()))?;
    if !metadata.is_file() {
        bail!("GPG 口令文件不是普通文件: {}", path.display());
    }
    if metadata.len() == 0 {
        bail!("GPG 口令文件为空: {}", path.display());
    }

    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        util::warn(format!(
            "GPG 口令文件 {} 的权限为 {mode:o}，建议收紧为 0600",
            path.display()
        ));
    }

    let status = runner
        .command("test")?
        .arg("-r")
        .arg(path)
        .status()
        .context("检查口令文件可读性失败")?;
    if !status.success() {
        bail!(
            "用户 {} 无法读取 GPG 口令文件 {}",
            runner.user.name,
            path.display()
        );
    }
    Ok(())
}

fn list_secret_fingerprints(runner: &Runner, key: Option<&str>) -> Result<Vec<String>> {
    let mut command = runner.command("gpg")?;
    command.args(["--batch", "--with-colons", "--list-secret-keys"]);
    if let Some(key) = key {
        command.arg(key);
    }

    let output = command.output().context("执行 gpg 列出私钥失败")?;
    if !output.status.success() {
        bail!(
            "无法列出用户 {} 的 GPG 私钥: {}",
            runner.user.name,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let mut fingerprints = Vec::new();
    let mut expect_fingerprint = false;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let fields: Vec<&str> = line.split(':').collect();
        match fields.first().copied() {
            Some("sec") | Some("ssb") => expect_fingerprint = true,
            Some("fpr") if expect_fingerprint => {
                if let Some(fingerprint) = fields.get(9).filter(|value| !value.is_empty()) {
                    fingerprints.push(fingerprint.to_string());
                }
                expect_fingerprint = false;
            }
            _ => {}
        }
    }

    if fingerprints.is_empty() {
        bail!(
            "用户 {} 的 GPG 密钥环中没有可用的私钥（GPG 目录: {}）",
            runner.user.name,
            runner.gpg_home.display()
        );
    }
    Ok(fingerprints)
}
