use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::gpg::{Runner, Signer};
use crate::util;

#[derive(Debug, Clone)]
pub struct LocalPackage {
    pub version: String,
    pub filename: String,
    pub sha256: Option<String>,
}

pub fn validate_db_path(path: &Path) -> Result<()> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    if !name.contains(".db.tar.") {
        bail!(
            "数据库路径必须类似 <name>.db.tar.zst 或 <name>.db.tar.gz: {}",
            path.display()
        );
    }
    Ok(())
}

/// Derive `foo.files.tar.zst` from `foo.db.tar.zst`.
pub fn files_db_path(db_path: &Path) -> Result<PathBuf> {
    let name = db_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("无效的数据库路径: {}", db_path.display()))?;
    if !name.contains(".db.tar.") {
        bail!("无法从 {} 推导 files 数据库路径", db_path.display());
    }
    Ok(db_path.with_file_name(name.replacen(".db.tar.", ".files.tar.", 1)))
}

/// Read package entries from the local repo-add database.
pub fn read_db(path: &Path) -> Result<HashMap<String, LocalPackage>> {
    let mut packages = HashMap::new();
    if !path.exists() {
        return Ok(packages);
    }

    let file = File::open(path).with_context(|| format!("打开数据库失败: {}", path.display()))?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    let reader: Box<dyn Read> = if name.ends_with(".zst") {
        Box::new(
            zstd::stream::read::Decoder::new(file)
                .with_context(|| format!("解压数据库失败: {}", path.display()))?,
        )
    } else if name.ends_with(".gz") {
        Box::new(flate2::read::GzDecoder::new(file))
    } else {
        bail!(
            "不支持的数据库压缩格式（仅支持 .zst 和 .gz）: {}",
            path.display()
        );
    };

    let mut archive = tar::Archive::new(reader);
    let entries = archive
        .entries()
        .with_context(|| format!("读取数据库 tar 失败: {}", path.display()))?;

    for entry in entries {
        let mut entry = entry.with_context(|| format!("读取数据库条目失败: {}", path.display()))?;
        let entry_path = entry
            .path()
            .with_context(|| format!("读取数据库条目路径失败: {}", path.display()))?
            .into_owned();
        if entry_path.file_name().and_then(|name| name.to_str()) != Some("desc") {
            continue;
        }

        let mut desc = String::new();
        entry
            .read_to_string(&mut desc)
            .with_context(|| format!("读取数据库条目 {} 失败", entry_path.display()))?;

        let (Some(pkgname), Some(version), Some(filename)) = (
            desc_field(&desc, "NAME"),
            desc_field(&desc, "VERSION"),
            desc_field(&desc, "FILENAME"),
        ) else {
            util::warn(format!(
                "数据库中条目 {} 缺少必要字段，已忽略",
                entry_path.display()
            ));
            continue;
        };

        let filename = match util::sanitize_filename(&filename) {
            Ok(filename) => filename,
            Err(error) => {
                util::warn(format!(
                    "数据库中的文件名不安全，已忽略 {pkgname}: {error:#}"
                ));
                continue;
            }
        };

        packages.insert(
            pkgname,
            LocalPackage {
                version,
                filename,
                sha256: desc_field(&desc, "SHA256SUM").map(|sha| sha.to_ascii_lowercase()),
            },
        );
    }

    Ok(packages)
}

pub fn desc_field(desc: &str, key: &str) -> Option<String> {
    let marker = format!("%{key}%");
    let mut lines = desc.lines();
    while let Some(line) = lines.next() {
        if line.trim() == marker {
            let mut values = Vec::new();
            for value in lines.by_ref() {
                if value.trim().is_empty() {
                    break;
                }
                values.push(value);
            }
            if values.is_empty() {
                return None;
            }
            return Some(values.join("\n"));
        }
    }
    None
}

pub fn repo_add(
    runner: &Runner,
    db_path: &Path,
    packages: &[PathBuf],
    allow_downgrade: bool,
) -> Result<()> {
    let mut command = runner.command("repo-add")?;
    command.arg("--nocolor");
    if !allow_downgrade {
        command.arg("--prevent-downgrade");
    }
    command.arg(db_path);
    for package in packages {
        command.arg(package);
    }

    let output = command.output().context("执行 repo-add 失败")?;
    if !output.status.success() {
        bail!(
            "repo-add 失败:\n{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    if util::verbose() {
        print!("{}", String::from_utf8_lossy(&output.stdout));
        print!("{}", String::from_utf8_lossy(&output.stderr));
    }
    Ok(())
}

#[derive(Debug, Default)]
pub struct SignResult {
    pub db_signed: bool,
    pub files_signed: bool,
}

/// Sign the database files if their signatures are missing or invalid.
/// `repo-add` is run without `--sign` so that signing can use an explicit
/// passphrase file; this restores the signatures afterwards.
pub fn sign_databases(signer: &Signer, db_path: &Path) -> Result<SignResult> {
    let files_path = files_db_path(db_path)?;
    let mut result = SignResult::default();

    if db_path.exists() {
        result.db_signed = ensure_signature(signer, db_path)?;
    } else {
        util::debug(format!("数据库尚不存在，跳过签名: {}", db_path.display()));
    }

    if files_path.exists() {
        result.files_signed = ensure_signature(signer, &files_path)?;
    } else {
        util::debug(format!(
            "files 数据库不存在，跳过签名: {}",
            files_path.display()
        ));
    }

    if db_path.exists() {
        ensure_sig_symlink(&signer.runner, db_path)?;
    }
    if files_path.exists() {
        ensure_sig_symlink(&signer.runner, &files_path)?;
    }

    Ok(result)
}

fn ensure_signature(signer: &Signer, file: &Path) -> Result<bool> {
    let signature = util::sig_path(file);
    if signature.is_file() && signer.verify(&signature, file)? {
        util::debug(format!("签名有效: {}", signature.display()));
        return Ok(false);
    }

    if signature.exists() {
        util::warn(format!(
            "签名无效或不属于仓库密钥，重新签名: {}",
            file.display()
        ));
    }

    util::info(format!(
        "签名数据库 {}",
        file.file_name().unwrap_or_default().to_string_lossy()
    ));
    let signature = signer.sign(file)?;
    if !signer.verify(&signature, file)? {
        bail!("数据库签名验证失败: {}", file.display());
    }
    Ok(true)
}

fn ensure_sig_symlink(runner: &Runner, file: &Path) -> Result<()> {
    let name = file
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("无效的数据库路径: {}", file.display()))?;
    let base = name
        .split_once(".tar.")
        .map(|(base, _)| base)
        .ok_or_else(|| anyhow::anyhow!("无法推导签名链接名: {}", file.display()))?;
    let link = file.with_file_name(format!("{base}.sig"));
    let target = format!("{name}.sig");

    if let Ok(existing) = fs::read_link(&link)
        && existing == Path::new(&target)
    {
        runner.chown_symlink(&link)?;
        return Ok(());
    }

    match fs::symlink_metadata(&link) {
        Ok(_) => {
            fs::remove_file(&link)
                .with_context(|| format!("删除旧签名链接 {} 失败", link.display()))?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("检查 {} 失败", link.display()));
        }
    }

    std::os::unix::fs::symlink(&target, &link)
        .with_context(|| format!("创建签名链接 {} -> {} 失败", link.display(), target))?;
    runner.chown_symlink(&link)?;
    Ok(())
}

/// Verify and, if needed, re-sign an already installed package.
pub fn ensure_package_signature(
    signer: &Signer,
    repo_dir: &Path,
    package: &LocalPackage,
) -> Result<bool> {
    let file = repo_dir.join(&package.filename);
    if !file.is_file() {
        bail!("本地软件包文件不存在: {}", file.display());
    }

    let signature = util::sig_path(&file);
    if signature.is_file() && signer.verify(&signature, &file)? {
        return Ok(false);
    }

    if signature.exists() {
        util::warn(format!(
            "签名无效或不属于仓库密钥，重新签名: {}",
            file.display()
        ));
    }

    util::info(format!("补签软件包 {}", package.filename));
    let signature = signer.sign(&file)?;
    if !signer.verify(&signature, &file)? {
        bail!("软件包签名验证失败: {}", file.display());
    }
    Ok(true)
}

/// Move a downloaded file from the staging directory into the repository.
pub fn install_file(
    runner: &Runner,
    source: &Path,
    repo_dir: &Path,
    filename: &str,
) -> Result<PathBuf> {
    let destination = repo_dir.join(filename);
    fs::rename(source, &destination)
        .with_context(|| format!("安装 {} 失败", destination.display()))?;
    fs::set_permissions(&destination, fs::Permissions::from_mode(0o644))
        .with_context(|| format!("设置 {} 权限失败", destination.display()))?;
    runner.chown(&destination)?;
    Ok(destination)
}

/// Delete the previous package file (and signature) for entries that were
/// replaced, unless another database entry still references it.
pub fn cleanup_replaced(
    repo_dir: &Path,
    db: &HashMap<String, LocalPackage>,
    replaced: &[(String, String)],
) -> Result<()> {
    let mut references: HashMap<&str, usize> = HashMap::new();
    for package in db.values() {
        *references.entry(package.filename.as_str()).or_default() += 1;
    }

    for (new_filename, old_filename) in replaced {
        if new_filename == old_filename {
            continue;
        }
        if references.get(old_filename.as_str()).copied().unwrap_or(0) > 1 {
            util::warn(format!(
                "旧文件 {old_filename} 仍被其他数据库条目引用，保留"
            ));
            continue;
        }

        for path in [
            repo_dir.join(old_filename),
            util::sig_path(&repo_dir.join(old_filename)),
        ] {
            if !path.exists() {
                continue;
            }
            match fs::remove_file(&path) {
                Ok(()) => util::detail(format!(
                    "已删除旧文件 {}",
                    path.file_name().unwrap_or_default().to_string_lossy()
                )),
                Err(error) => util::warn(format!("删除旧文件 {} 失败: {error}", path.display())),
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DESC: &str = "%FILENAME%\nfnm-bin-1.39.0-1-x86_64.pkg.tar.zst\n\n%NAME%\nfnm-bin\n\n%VERSION%\n1.39.0-1\n\n%SHA256SUM%\nae34b207417e5fec55d88ee0163254ff477cf5193c4f1e2b2e17599c83b97569\n\n";

    #[test]
    fn parses_desc_fields() {
        assert_eq!(desc_field(DESC, "NAME").as_deref(), Some("fnm-bin"));
        assert_eq!(desc_field(DESC, "VERSION").as_deref(), Some("1.39.0-1"));
        assert_eq!(
            desc_field(DESC, "FILENAME").as_deref(),
            Some("fnm-bin-1.39.0-1-x86_64.pkg.tar.zst")
        );
        assert_eq!(desc_field(DESC, "MISSING"), None);
    }

    #[test]
    fn derives_files_database_path() {
        assert_eq!(
            files_db_path(Path::new("/repo/ayaya.db.tar.zst")).unwrap(),
            PathBuf::from("/repo/ayaya.files.tar.zst")
        );
        assert_eq!(
            files_db_path(Path::new("/repo/foo.db.tar.gz")).unwrap(),
            PathBuf::from("/repo/foo.files.tar.gz")
        );
        assert!(files_db_path(Path::new("/repo/foo.db")).is_err());
    }

    #[test]
    fn validates_database_path() {
        assert!(validate_db_path(Path::new("/repo/ayaya.db.tar.zst")).is_ok());
        assert!(validate_db_path(Path::new("/repo/ayaya.db")).is_err());
    }
}
