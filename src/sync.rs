use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;
use sha2::{Digest, Sha256};

use crate::gpg::{Runner, Signer};
use crate::local_repo::{self, LocalPackage};
use crate::manifest::{Manifest, ManifestPackage};
use crate::util;

const MAX_PACKAGE_BYTES: u64 = 8 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    New,
    Update,
    Rebuild,
    UpToDate,
    Downgrade,
}

#[derive(Debug, Clone)]
pub struct PlannedPackage {
    pub remote: ManifestPackage,
    pub local: Option<LocalPackage>,
    pub action: Action,
}

#[derive(Debug)]
pub struct Plan {
    pub packages: Vec<PlannedPackage>,
    pub other_arch: usize,
}

#[derive(Debug, Default)]
pub struct Summary {
    pub downloaded: usize,
    pub up_to_date: usize,
    pub downgraded: usize,
    pub packages_resigned: usize,
    pub failures: usize,
    pub database_signed: bool,
}

pub struct SyncOptions<'a> {
    pub repo_dir: &'a Path,
    pub db_path: &'a Path,
    pub db: &'a HashMap<String, LocalPackage>,
    pub runner: &'a Runner,
    pub signer: Option<&'a Signer>,
    pub timeout: Duration,
    pub allow_downgrade: bool,
    pub keep_old: bool,
}

pub fn build_plan(
    manifest: &Manifest,
    db: &HashMap<String, LocalPackage>,
    arch: &str,
    allow_downgrade: bool,
    repo_dir: &Path,
) -> Result<Plan> {
    let mut packages = Vec::new();
    let mut seen = HashSet::new();
    let mut other_arch = 0;

    for remote in &manifest.packages {
        if remote.arch != arch {
            other_arch += 1;
            continue;
        }
        if remote.pkgname.is_empty() {
            bail!("manifest 中存在空的包名");
        }
        if !seen.insert(remote.pkgname.clone()) {
            bail!("manifest 中包名重复: {}", remote.pkgname);
        }

        let local = db.get(&remote.pkgname).cloned();
        let file_missing = local
            .as_ref()
            .is_some_and(|local| !repo_dir.join(&local.filename).is_file());

        let mut action = decide(remote, local.as_ref(), allow_downgrade, util::vercmp)?;
        if file_missing && let Some(local) = &local {
            if action == Action::Downgrade {
                util::warn(format!(
                    "数据库中登记的软件包文件缺失，且 manifest 版本更低，跳过: {}",
                    local.filename
                ));
            } else {
                util::warn(format!(
                    "数据库中登记的软件包文件缺失，将重新下载: {}",
                    local.filename
                ));
                // Keep the version comparison authoritative: a missing file
                // for an otherwise up-to-date package is a re-download.
                if action == Action::UpToDate {
                    action = Action::Update;
                }
            }
        }
        packages.push(PlannedPackage {
            remote: remote.clone(),
            local,
            action,
        });
    }

    Ok(Plan {
        packages,
        other_arch,
    })
}

fn decide(
    remote: &ManifestPackage,
    local: Option<&LocalPackage>,
    allow_downgrade: bool,
    compare: impl Fn(&str, &str) -> Result<Ordering>,
) -> Result<Action> {
    let Some(local) = local else {
        return Ok(Action::New);
    };

    Ok(match compare(&remote.version, &local.version)? {
        Ordering::Equal => match (&remote.sha256, &local.sha256) {
            (Some(remote_sha), Some(local_sha)) if !remote_sha.eq_ignore_ascii_case(local_sha) => {
                Action::Rebuild
            }
            _ => Action::UpToDate,
        },
        Ordering::Greater => Action::Update,
        Ordering::Less if allow_downgrade => Action::Update,
        Ordering::Less => Action::Downgrade,
    })
}

pub fn print_plan(plan: &Plan) {
    if plan.other_arch > 0 {
        util::detail(format!("忽略 {} 个其他架构的软件包", plan.other_arch));
    }
    if plan.packages.is_empty() {
        util::info("manifest 中没有目标架构的软件包");
        return;
    }

    let mut actionable = 0;
    for item in &plan.packages {
        match item.action {
            Action::New => {
                actionable += 1;
                util::detail(format!(
                    "[新增] {} {}",
                    item.remote.pkgname, item.remote.version
                ));
            }
            Action::Update => {
                actionable += 1;
                util::detail(format!(
                    "[更新] {} {} -> {}",
                    item.remote.pkgname,
                    item.local
                        .as_ref()
                        .map(|local| local.version.as_str())
                        .unwrap_or("?"),
                    item.remote.version
                ));
            }
            Action::Rebuild => {
                actionable += 1;
                util::detail(format!(
                    "[重建] {} {}（sha256 变化）",
                    item.remote.pkgname, item.remote.version
                ));
            }
            Action::Downgrade => util::warn(format!(
                "跳过降级: {} 本地 {}，manifest {}（可用 --allow-downgrade 强制）",
                item.remote.pkgname,
                item.local
                    .as_ref()
                    .map(|local| local.version.as_str())
                    .unwrap_or("?"),
                item.remote.version
            )),
            Action::UpToDate => {
                util::debug(format!(
                    "[最新] {} {}",
                    item.remote.pkgname, item.remote.version
                ));
            }
        }
    }

    if actionable == 0 {
        util::info("所有软件包均为最新");
    } else {
        util::info(format!("共 {actionable} 个软件包需要同步"));
    }
}

pub fn execute(plan: &Plan, options: &SyncOptions) -> Result<Summary> {
    let mut summary = Summary::default();
    for item in &plan.packages {
        match item.action {
            Action::UpToDate => summary.up_to_date += 1,
            Action::Downgrade => summary.downgraded += 1,
            _ => {}
        }
    }

    let needs_download = plan
        .packages
        .iter()
        .any(|item| matches!(item.action, Action::New | Action::Update | Action::Rebuild));

    // Keep the staging directory alive until the prepared files have been
    // moved into the repository.
    let staging = if needs_download {
        Some(StagingDir::create(options.repo_dir)?)
    } else {
        None
    };

    let mut prepared = Vec::new();
    if let Some(staging) = &staging {
        options.runner.prepare_staging(&staging.path)?;
        let client = build_client(options.timeout)?;

        for item in &plan.packages {
            if !matches!(item.action, Action::New | Action::Update | Action::Rebuild) {
                continue;
            }
            match prepare_package(&client, item, options, staging) {
                Ok(package) => prepared.push(package),
                Err(error) => {
                    util::error(format!("{}: {error:#}", item.remote.pkgname));
                    summary.failures += 1;
                }
            }
        }
    }

    // Only packages whose file and signature are both installed may be added
    // to the database.
    let mut installed: Vec<&PreparedPackage> = Vec::new();
    for package in &prepared {
        match install_prepared(package, options) {
            Ok(()) => installed.push(package),
            Err(error) => {
                util::error(format!("{}: {error:#}", package.pkgname));
                summary.failures += 1;
            }
        }
    }
    summary.downloaded = installed.len();

    if !installed.is_empty() {
        let paths: Vec<PathBuf> = installed
            .iter()
            .map(|package| options.repo_dir.join(&package.filename))
            .collect();
        util::info(format!("运行 repo-add 添加 {} 个软件包...", paths.len()));
        local_repo::repo_add(
            options.runner,
            options.db_path,
            &paths,
            options.allow_downgrade,
        )?;

        if !options.keep_old {
            let replaced: Vec<(String, String)> = installed
                .iter()
                .filter_map(|package| {
                    package
                        .local
                        .as_ref()
                        .map(|local| (package.filename.clone(), local.filename.clone()))
                })
                .collect();
            local_repo::cleanup_replaced(options.repo_dir, options.db, &replaced)?;
        }
    }

    // repo-add is deliberately run without --sign so signing can use the
    // configured passphrase file. Sign the databases here, and heal a missing
    // or invalid signature even when no package changed.
    if let Some(signer) = options.signer {
        match local_repo::sign_databases(signer, options.db_path) {
            Ok(result) => summary.database_signed = result.db_signed || result.files_signed,
            Err(error) => {
                util::error("数据库签名失败，仓库当前可能没有有效签名");
                return Err(error);
            }
        }
    }

    // Ensure packages already present locally also have a valid signature from
    // this repository's key.
    if let Some(signer) = options.signer {
        for item in &plan.packages {
            if !matches!(item.action, Action::UpToDate | Action::Downgrade) {
                continue;
            }
            let Some(local) = &item.local else {
                continue;
            };
            match local_repo::ensure_package_signature(signer, options.repo_dir, local) {
                Ok(true) => summary.packages_resigned += 1,
                Ok(false) => {}
                Err(error) => {
                    util::error(format!("{}: {error:#}", item.remote.pkgname));
                    summary.failures += 1;
                }
            }
        }
    }

    print_summary(&summary);
    if summary.failures > 0 {
        bail!("{} 个软件包处理失败", summary.failures);
    }
    Ok(summary)
}

fn print_summary(summary: &Summary) {
    let database = if summary.database_signed {
        "，数据库已重新签名"
    } else {
        ""
    };
    util::info(format!(
        "完成：同步 {}，已是最新 {}，降级跳过 {}，补签 {}，失败 {}{database}",
        summary.downloaded,
        summary.up_to_date,
        summary.downgraded,
        summary.packages_resigned,
        summary.failures
    ));
}

struct PreparedPackage {
    pkgname: String,
    filename: String,
    path: PathBuf,
    signature: Option<PathBuf>,
    local: Option<LocalPackage>,
}

fn prepare_package(
    client: &Client,
    item: &PlannedPackage,
    options: &SyncOptions,
    staging: &StagingDir,
) -> Result<PreparedPackage> {
    let filename = util::sanitize_filename(&item.remote.filename)?;
    let sha256 = item
        .remote
        .sha256
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("manifest 中缺少 sha256，拒绝下载和签名"))?;
    if sha256.len() != 64 || !sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("manifest 中的 sha256 格式无效: {sha256}");
    }

    util::info(format!(
        "下载 {} {}（{}）",
        item.remote.pkgname,
        item.remote.version,
        item.remote
            .size
            .map(util::format_size)
            .unwrap_or_else(|| "大小未知".to_string())
    ));

    let path = staging.path.join(&filename);
    download_package(client, &item.remote, &path, sha256)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644))
        .with_context(|| format!("设置 {} 权限失败", path.display()))?;

    let signature = match options.signer {
        Some(signer) => {
            util::info(format!("签名 {}", item.remote.pkgname));
            let signature = signer.sign(&path)?;
            if !signer.verify(&signature, &path)? {
                bail!("新生成的软件包签名验证失败");
            }
            Some(signature)
        }
        None => None,
    };

    Ok(PreparedPackage {
        pkgname: item.remote.pkgname.clone(),
        filename,
        path,
        signature,
        local: item.local.clone(),
    })
}

fn install_prepared(package: &PreparedPackage, options: &SyncOptions) -> Result<()> {
    local_repo::install_file(
        options.runner,
        &package.path,
        options.repo_dir,
        &package.filename,
    )?;

    if let Some(signature) = &package.signature {
        let signature_name = format!("{}.sig", package.filename);
        local_repo::install_file(options.runner, signature, options.repo_dir, &signature_name)?;
    }
    Ok(())
}

fn build_client(timeout: Duration) -> Result<Client> {
    Client::builder()
        .user_agent(concat!("arch-repo-update/", env!("CARGO_PKG_VERSION")))
        .timeout(timeout)
        .build()
        .context("创建 HTTP 客户端失败")
}

fn download_package(
    client: &Client,
    remote: &ManifestPackage,
    path: &Path,
    sha256: &str,
) -> Result<()> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        match try_download(client, remote, path, sha256) {
            Ok(()) => return Ok(()),
            Err(error) if attempt < 3 => {
                util::warn(format!(
                    "下载 {} 失败（第 {attempt} 次），2 秒后重试: {error:#}",
                    remote.pkgname
                ));
                std::thread::sleep(Duration::from_secs(2));
            }
            Err(error) => return Err(error),
        }
    }
}

fn try_download(
    client: &Client,
    remote: &ManifestPackage,
    path: &Path,
    sha256: &str,
) -> Result<()> {
    let part = {
        let mut name = path.as_os_str().to_os_string();
        name.push(".part");
        PathBuf::from(name)
    };

    let result = (|| -> Result<()> {
        let mut response = client
            .get(&remote.url)
            .send()
            .with_context(|| format!("请求 {} 失败", remote.url))?
            .error_for_status()
            .with_context(|| format!("下载 {} 返回错误状态", remote.url))?;

        // The manifest size is an upper bound and is also checked exactly
        // after the download. Without it, cap the response at 8 GiB.
        let limit = remote.size.unwrap_or(MAX_PACKAGE_BYTES);
        let mut file =
            fs::File::create(&part).with_context(|| format!("创建 {} 失败", part.display()))?;
        let mut hasher = Sha256::new();
        let mut total: u64 = 0;
        let mut buffer = vec![0u8; 128 * 1024];

        loop {
            let read = response.read(&mut buffer).context("读取下载数据失败")?;
            if read == 0 {
                break;
            }
            total += read as u64;
            if total > limit {
                bail!("下载内容超过 manifest 声明的大小（{limit} 字节）");
            }
            hasher.update(&buffer[..read]);
            file.write_all(&buffer[..read])
                .context("写入下载文件失败")?;
        }

        file.sync_all().context("同步下载文件失败")?;
        drop(file);

        if let Some(expected) = remote.size
            && total != expected
        {
            bail!("下载大小不匹配：期望 {expected} 字节，实际 {total} 字节");
        }

        let digest = hex::encode(hasher.finalize());
        if !digest.eq_ignore_ascii_case(sha256) {
            bail!("sha256 校验失败：期望 {sha256}，实际 {digest}");
        }

        fs::rename(&part, path).with_context(|| format!("重命名 {} 失败", part.display()))?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&part);
    }
    result
}

struct StagingDir {
    path: PathBuf,
}

impl StagingDir {
    fn create(repo_dir: &Path) -> Result<Self> {
        // We hold the repository lock, so any leftover staging directory from
        // an earlier crashed run can be removed safely.
        for entry in fs::read_dir(repo_dir)
            .with_context(|| format!("读取仓库目录 {} 失败", repo_dir.display()))?
        {
            let entry = entry?;
            if !entry
                .file_name()
                .to_string_lossy()
                .starts_with(".arch-repo-update.staging.")
            {
                continue;
            }
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)
                .with_context(|| format!("读取 {} 的元数据失败", path.display()))?;
            if !metadata.file_type().is_dir() {
                continue;
            }
            util::debug(format!("清理旧的临时目录 {}", path.display()));
            if let Err(error) = fs::remove_dir_all(&path) {
                util::warn(format!("清理旧的临时目录 {} 失败: {error}", path.display()));
            }
        }

        let path = repo_dir.join(format!(".arch-repo-update.staging.{}", std::process::id()));
        if path.exists() {
            fs::remove_dir_all(&path)
                .with_context(|| format!("清理旧的临时目录 {} 失败", path.display()))?;
        }
        fs::create_dir(&path).with_context(|| format!("创建临时目录 {} 失败", path.display()))?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
            .with_context(|| format!("设置 {} 权限失败", path.display()))?;
        Ok(Self { path })
    }
}

impl Drop for StagingDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote(version: &str, sha256: &str) -> ManifestPackage {
        ManifestPackage {
            name: None,
            pkgname: "demo".to_string(),
            version: version.to_string(),
            arch: "x86_64".to_string(),
            filename: format!("demo-{version}-x86_64.pkg.tar.zst"),
            sha256: Some(sha256.to_string()),
            size: None,
            url: "https://example.invalid/demo.pkg.tar.zst".to_string(),
            built_at: None,
            asset_id: None,
            asset_updated_at: None,
        }
    }

    fn local(version: &str, sha256: &str) -> LocalPackage {
        LocalPackage {
            version: version.to_string(),
            filename: format!("demo-{version}-x86_64.pkg.tar.zst"),
            sha256: Some(sha256.to_string()),
        }
    }

    fn fake_compare(a: &str, b: &str) -> Result<Ordering> {
        Ok(a.cmp(b))
    }

    #[test]
    fn classifies_updates() {
        let sha = "a".repeat(64);
        assert_eq!(
            decide(&remote("1.0-1", &sha), None, false, fake_compare).unwrap(),
            Action::New
        );
        assert_eq!(
            decide(
                &remote("2.0-1", &sha),
                Some(&local("1.0-1", &sha)),
                false,
                fake_compare
            )
            .unwrap(),
            Action::Update
        );
        assert_eq!(
            decide(
                &remote("1.0-1", &sha),
                Some(&local("1.0-1", &sha)),
                false,
                fake_compare
            )
            .unwrap(),
            Action::UpToDate
        );
        assert_eq!(
            decide(
                &remote("1.0-1", &"b".repeat(64)),
                Some(&local("1.0-1", &sha)),
                false,
                fake_compare
            )
            .unwrap(),
            Action::Rebuild
        );
        assert_eq!(
            decide(
                &remote("1.0-1", &sha),
                Some(&local("2.0-1", &sha)),
                false,
                fake_compare
            )
            .unwrap(),
            Action::Downgrade
        );
        assert_eq!(
            decide(
                &remote("1.0-1", &sha),
                Some(&local("2.0-1", &sha)),
                true,
                fake_compare
            )
            .unwrap(),
            Action::Update
        );
    }

    #[test]
    fn missing_local_file_is_redownloaded() {
        let dir = std::env::temp_dir().join(format!(
            "arch-repo-update-plan-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();

        let sha = "a".repeat(64);
        let manifest = Manifest {
            generated_at: None,
            release: None,
            packages: vec![remote("1.0-1", &sha)],
        };

        let mut db = HashMap::new();
        db.insert("demo".to_string(), local("1.0-1", &sha));
        let plan = build_plan(&manifest, &db, "x86_64", false, &dir).unwrap();
        assert_eq!(plan.packages[0].action, Action::Update);

        // A missing local file must not turn an older remote version into an
        // upgrade.
        let mut db = HashMap::new();
        db.insert("demo".to_string(), local("2.0-1", &sha));
        let plan = build_plan(&manifest, &db, "x86_64", false, &dir).unwrap();
        assert_eq!(plan.packages[0].action, Action::Downgrade);

        fs::remove_dir_all(&dir).unwrap();
    }
}
