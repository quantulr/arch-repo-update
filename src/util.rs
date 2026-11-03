use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};

static VERBOSE: AtomicBool = AtomicBool::new(false);

pub fn set_verbose(value: bool) {
    VERBOSE.store(value, Ordering::Relaxed);
}

pub fn verbose() -> bool {
    VERBOSE.load(Ordering::Relaxed)
}

pub fn info(message: impl AsRef<str>) {
    println!("==> {}", message.as_ref());
}

pub fn detail(message: impl AsRef<str>) {
    println!("    {}", message.as_ref());
}

pub fn warn(message: impl AsRef<str>) {
    eprintln!("警告: {}", message.as_ref());
}

pub fn error(message: impl AsRef<str>) {
    eprintln!("错误: {}", message.as_ref());
}

pub fn debug(message: impl AsRef<str>) {
    if verbose() {
        println!("[debug] {}", message.as_ref());
    }
}

pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Validate a file name taken from the manifest.
///
/// The manifest is remote input. The name is later joined to a directory and
/// handed to `repo-add`, so reject anything that could escape the directory or
/// be interpreted as an option.
pub fn sanitize_filename(name: &str) -> Result<String> {
    if name.is_empty() {
        bail!("manifest 中的文件名为空");
    }
    if name.starts_with('-') {
        bail!("文件名不能以 '-' 开头: {name}");
    }
    if name.contains('/') || name.contains('\\') || name.contains('\0') {
        bail!("文件名包含路径分隔符: {name}");
    }
    if name == "." || name == ".." {
        bail!("非法文件名: {name}");
    }

    const SUFFIXES: [&str; 4] = [".pkg.tar.zst", ".pkg.tar.xz", ".pkg.tar.gz", ".pkg.tar.bz2"];
    if !SUFFIXES.iter().any(|suffix| name.ends_with(suffix)) {
        bail!("不是受支持的软件包文件名: {name}");
    }

    Ok(name.to_string())
}

const MAX_MANIFEST_BYTES: u64 = 16 * 1024 * 1024;

/// Read the manifest from an HTTP(S) URL, a `file://` URL, or a local path.
pub fn fetch_manifest(source: &str, timeout: Duration) -> Result<Vec<u8>> {
    if let Some(path) = source.strip_prefix("file://") {
        return std::fs::read(path).with_context(|| format!("读取 manifest 失败: {path}"));
    }

    if source.starts_with("http://") || source.starts_with("https://") {
        let client = reqwest::blocking::Client::builder()
            .user_agent(concat!("arch-repo-update/", env!("CARGO_PKG_VERSION")))
            .timeout(timeout)
            .build()
            .context("创建 HTTP 客户端失败")?;

        let response = client
            .get(source)
            .send()
            .with_context(|| format!("请求 manifest 失败: {source}"))?
            .error_for_status()
            .with_context(|| format!("manifest 返回错误状态: {source}"))?;

        let mut body = Vec::new();
        response
            .take(MAX_MANIFEST_BYTES + 1)
            .read_to_end(&mut body)
            .context("读取 manifest 响应失败")?;
        if body.len() as u64 > MAX_MANIFEST_BYTES {
            bail!("manifest 超过 {MAX_MANIFEST_BYTES} 字节上限");
        }
        return Ok(body);
    }

    std::fs::read(source).with_context(|| format!("读取 manifest 失败: {source}"))
}

pub fn sig_path(file: &Path) -> PathBuf {
    let mut name = file.as_os_str().to_os_string();
    name.push(".sig");
    PathBuf::from(name)
}

/// Compare two package versions using pacman's `vercmp`.
pub fn vercmp(a: &str, b: &str) -> Result<std::cmp::Ordering> {
    let output = std::process::Command::new("vercmp")
        .env("LC_ALL", "C")
        .args([a, b])
        .output()
        .context("执行 vercmp 失败（该命令由 pacman 提供）")?;

    if !output.status.success() {
        bail!(
            "vercmp 执行失败: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    match String::from_utf8_lossy(&output.stdout).trim() {
        "-1" => Ok(std::cmp::Ordering::Less),
        "0" => Ok(std::cmp::Ordering::Equal),
        "1" => Ok(std::cmp::Ordering::Greater),
        other => bail!("无法解析 vercmp 输出: {other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering;

    #[test]
    fn accepts_normal_package_names() {
        assert_eq!(
            sanitize_filename("bili-FM-3.1.16-linux-x86_64.pkg.tar.zst").unwrap(),
            "bili-FM-3.1.16-linux-x86_64.pkg.tar.zst"
        );
        assert!(sanitize_filename("fnm-bin-1.39.0-1-x86_64.pkg.tar.zst").is_ok());
    }

    #[test]
    fn rejects_path_traversal_and_options() {
        assert!(sanitize_filename("../evil.pkg.tar.zst").is_err());
        assert!(sanitize_filename("a/b.pkg.tar.zst").is_err());
        assert!(sanitize_filename("a\\b.pkg.tar.zst").is_err());
        assert!(sanitize_filename("-rf.pkg.tar.zst").is_err());
        assert!(sanitize_filename("..").is_err());
        assert!(sanitize_filename("not-a-package.txt").is_err());
        assert!(sanitize_filename("").is_err());
    }

    #[test]
    fn formats_sizes() {
        assert_eq!(format_size(100), "100 B");
        assert_eq!(format_size(1536), "1.5 KiB");
        assert_eq!(format_size(3 * 1024 * 1024), "3.0 MiB");
    }

    #[test]
    fn compares_versions_with_vercmp() {
        assert_eq!(vercmp("1.0-1", "1.0-1").unwrap(), Ordering::Equal);
        assert_eq!(vercmp("1.0-2", "1.0-1").unwrap(), Ordering::Greater);
        assert_eq!(vercmp("1.0-1", "1.1").unwrap(), Ordering::Less);
    }
}
