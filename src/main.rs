mod gpg;
mod local_repo;
mod manifest;
mod sync;
mod util;

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Parser;
use fs2::FileExt;

const DEFAULT_MANIFEST_URL: &str =
    "https://github.com/quantulr/archlinux-repo/releases/download/packages/manifest.json";
const DEFAULT_REPO_DIR: &str = "/srv/http/repo/ayaya/os/x86_64";
const DEFAULT_DB_NAME: &str = "ayaya";

fn default_arch() -> String {
    std::env::consts::ARCH.to_string()
}

#[derive(Debug, Parser)]
#[command(
    name = "arch-repo-update",
    version,
    about = "从 archlinux-repo 的 GitHub release manifest 同步 pacman 仓库，自动 repo-add 并签名",
    after_help = "示例:\n  sudo arch-repo-update --passphrase-file /etc/arch-repo-update/passphrase\n  arch-repo-update --dry-run"
)]
struct Args {
    /// 仓库目录（软件包与数据库所在目录）
    #[arg(long, default_value = DEFAULT_REPO_DIR)]
    repo_dir: PathBuf,

    /// 数据库名称，生成 <name>.db.tar.zst
    #[arg(long, default_value = DEFAULT_DB_NAME)]
    db_name: String,

    /// 直接指定数据库路径（覆盖 --repo-dir 与 --db-name）
    #[arg(long)]
    db_path: Option<PathBuf>,

    /// manifest 的 URL、file:// URL 或本地路径
    #[arg(long, default_value = DEFAULT_MANIFEST_URL)]
    manifest: String,

    /// 目标架构
    #[arg(long, default_value_t = default_arch())]
    arch: String,

    /// 运行 gpg 与 repo-add 的用户
    #[arg(long, default_value = "archrepo")]
    sign_user: String,

    /// GPG 主目录（默认为签名用户的 ~/.gnupg）
    #[arg(long)]
    gpg_home: Option<PathBuf>,

    /// GPG 口令文件；私钥受口令保护时必须提供
    #[arg(long)]
    passphrase_file: Option<PathBuf>,

    /// 指定签名密钥（指纹、key ID 或 UID）
    #[arg(long)]
    key: Option<String>,

    /// 不签名，仅下载并更新数据库（调试用，会使数据库失去签名）
    #[arg(long)]
    no_sign: bool,

    /// 允许降级到 manifest 中的旧版本
    #[arg(long)]
    allow_downgrade: bool,

    /// 保留被替换的旧版本软件包文件
    #[arg(long)]
    keep_old: bool,

    /// 只显示同步计划，不下载、不签名、不修改仓库
    #[arg(long)]
    dry_run: bool,

    /// 将仓库目录内现有文件的所有者修正为签名用户（需要 root）
    #[arg(long)]
    fix_ownership: bool,

    /// HTTP 超时（秒）
    #[arg(long, default_value_t = 600)]
    timeout: u64,

    /// 输出详细日志
    #[arg(long, short)]
    verbose: bool,
}

fn main() {
    let args = Args::parse();
    util::set_verbose(args.verbose);
    if let Err(error) = run(args) {
        util::error(format!("{error:#}"));
        std::process::exit(1);
    }
}

fn run(args: Args) -> Result<()> {
    if args.timeout == 0 {
        bail!("--timeout 必须大于 0");
    }
    if args.db_name.contains('/') || args.db_name.contains('\\') {
        bail!("--db-name 不能包含路径分隔符");
    }

    let (repo_dir, db_path) = resolve_paths(&args)?;

    if args.dry_run {
        if args.fix_ownership {
            bail!("--fix-ownership 不能与 --dry-run 一起使用");
        }
        util::info(format!("获取 manifest: {}", args.manifest));
        let manifest = manifest::load(&args.manifest, Duration::from_secs(args.timeout))?;
        let db = local_repo::read_db(&db_path)?;
        let plan = sync::build_plan(&manifest, &db, &args.arch, args.allow_downgrade, &repo_dir)?;
        sync::print_plan(&plan);
        util::info("dry-run：未下载、未签名、未修改仓库");
        return Ok(());
    }

    let runner = gpg::Runner::new(&args.sign_user, args.gpg_home.clone())?;
    if !runner.can_switch_to_user() {
        bail!(
            "请以 root 或用户 {} 运行（当前 uid 为 {}）",
            runner.user.name,
            gpg::current_euid()
        );
    }

    let _lock = lock_repo(&repo_dir)?;

    if args.fix_ownership {
        runner.fix_ownership(&repo_dir)?;
    }
    runner.check_writable(&repo_dir)?;
    if let Some(parent) = db_path.parent() {
        runner.check_writable(parent)?;
    }

    let signer = if args.no_sign {
        None
    } else {
        Some(gpg::Signer::new(
            runner.clone(),
            args.key.as_deref(),
            args.passphrase_file.clone(),
        )?)
    };

    util::info(format!("获取 manifest: {}", args.manifest));
    let manifest = manifest::load(&args.manifest, Duration::from_secs(args.timeout))?;
    if let Some(generated_at) = &manifest.generated_at {
        util::debug(format!("manifest 生成时间: {generated_at}"));
    }
    if let Some(release) = &manifest.release {
        util::debug(format!("manifest release: {release}"));
    }

    let db = local_repo::read_db(&db_path)?;
    util::debug(format!("本地数据库包含 {} 个软件包", db.len()));

    let plan = sync::build_plan(&manifest, &db, &args.arch, args.allow_downgrade, &repo_dir)?;
    sync::print_plan(&plan);

    let options = sync::SyncOptions {
        repo_dir: &repo_dir,
        db_path: &db_path,
        db: &db,
        runner: &runner,
        signer: signer.as_ref(),
        timeout: Duration::from_secs(args.timeout),
        allow_downgrade: args.allow_downgrade,
        keep_old: args.keep_old,
    };
    sync::execute(&plan, &options)?;

    Ok(())
}

fn resolve_paths(args: &Args) -> Result<(PathBuf, PathBuf)> {
    let repo_dir = fs::canonicalize(&args.repo_dir)
        .with_context(|| format!("仓库目录不存在: {}", args.repo_dir.display()))?;
    if !repo_dir.is_dir() {
        bail!("仓库路径不是目录: {}", repo_dir.display());
    }

    let db_path = match &args.db_path {
        Some(path) => {
            if path.exists() {
                fs::canonicalize(path)
                    .with_context(|| format!("解析数据库路径失败: {}", path.display()))?
            } else {
                let file_name = path
                    .file_name()
                    .ok_or_else(|| anyhow::anyhow!("无效的数据库路径: {}", path.display()))?;
                let parent = path
                    .parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                    .unwrap_or_else(|| Path::new("."));
                fs::canonicalize(parent)
                    .with_context(|| format!("数据库目录不存在: {}", parent.display()))?
                    .join(file_name)
            }
        }
        None => repo_dir.join(format!("{}.db.tar.zst", args.db_name)),
    };

    local_repo::validate_db_path(&db_path)?;
    Ok((repo_dir, db_path))
}

fn lock_repo(repo_dir: &Path) -> Result<File> {
    let file = File::open(repo_dir)
        .with_context(|| format!("打开仓库目录失败: {}", repo_dir.display()))?;
    file.try_lock_exclusive().map_err(|error| {
        anyhow::anyhow!("无法锁定仓库目录（可能已有另一个实例在运行）: {error}")
    })?;
    Ok(file)
}
