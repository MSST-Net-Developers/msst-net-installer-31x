//! Finding a release and downloading its artifacts safely.
//!
//! Downloads never touch the installation: each artifact is streamed into
//! `<staging>/<name>.part`, its size is capped at the manifest's size, its
//! SHA-256 is checked, and only then is it renamed to its final name inside
//! the staging directory.  A failure at any point leaves the installed
//! version exactly as it was.

use crate::fsutil;
use crate::manifest::{self, Artifact, MANIFEST_NAME, Manifest, Product, SIGNATURE_NAME, Trust, TrustPolicy};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

const USER_AGENT: &str = concat!("msst-net-installer/", env!("CARGO_PKG_VERSION"));

/// Release repositories: Core and the Controller are separate open-source
/// repositories, each publishing its own releases.
fn repo(source: &Source, product: Product) -> String {
    let (var, default) = match (source, product) {
        (Source::GitHub, Product::Core) => ("MSST_CORE_GITHUB_REPO", "MSST-Net-Developers/msst-net-core-31x"),
        (Source::GitHub, Product::Controller) => ("MSST_CONTROLLER_GITHUB_REPO", "MSST-Net-Developers/msst-net-controller-31x"),
        (_, Product::Core) => ("MSST_CORE_GITEE_REPO", "Abjust/msst-net-core-31x"),
        (_, Product::Controller) => ("MSST_CONTROLLER_GITEE_REPO", "Abjust/msst-net-controller-31x"),
    };
    std::env::var(var).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| default.to_string())
}
const RETRIES: u32 = 3;

#[derive(Clone, Debug)]
pub enum Source {
    GitHub,
    Gitee,
    /// A local directory with one release directory per product,
    /// `core/` and `controller/`, each holding `manifest.json`, its
    /// signature and the artifacts (offline installs, release testing).
    Dir(PathBuf),
}

impl Source {
    pub fn label(&self) -> String {
        match self {
            Source::GitHub => "GitHub".into(),
            Source::Gitee => "Gitee".into(),
            Source::Dir(p) => format!("本地目录 {}", p.display()),
        }
    }

    pub fn key(&self) -> String {
        match self {
            Source::GitHub => "github".into(),
            Source::Gitee => "gitee".into(),
            Source::Dir(p) => format!("dir:{}", p.display()),
        }
    }

    pub fn from_key(key: &str) -> Option<Source> {
        match key {
            "github" => Some(Source::GitHub),
            "gitee" => Some(Source::Gitee),
            _ => key.strip_prefix("dir:").map(|p| Source::Dir(PathBuf::from(p))),
        }
    }

    fn api_url(&self, product: Product, version: Option<&str>) -> Option<String> {
        let base = match self {
            Source::GitHub => "https://api.github.com/repos",
            Source::Gitee => "https://gitee.com/api/v5/repos",
            Source::Dir(_) => return None,
        };
        let repo = repo(self, product);
        Some(match version {
            Some(v) => format!("{base}/{repo}/releases/tags/v{}", v.trim_start_matches('v')),
            None => format!("{base}/{repo}/releases/latest"),
        })
    }
}

pub fn http_client() -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(600))
        .build()?)
}

#[derive(Deserialize)]
struct ApiRelease {
    tag_name: String,
    assets: Vec<ApiAsset>,
}

#[derive(Deserialize)]
struct ApiAsset {
    name: String,
    browser_download_url: String,
}

/// A release whose manifest has been verified.
pub struct Release {
    pub manifest: Manifest,
    pub trust: Trust,
    manifest_bytes: Vec<u8>,
    signature: Option<Vec<u8>>,
    locator: Locator,
}

enum Locator {
    Remote(HashMap<String, String>),
    Dir(PathBuf),
}

impl Release {
    /// Find a release of `product` (`version` = None: the latest) and
    /// verify its manifest.
    pub fn resolve(
        http: &reqwest::blocking::Client,
        source: &Source,
        product: Product,
        version: Option<&str>,
        policy: &TrustPolicy,
    ) -> Result<Release> {
        let (manifest_bytes, signature, locator) = match source {
            Source::Dir(root) => {
                let dir = root.join(dir_name(product));
                let bytes = std::fs::read(dir.join(MANIFEST_NAME))
                    .with_context(|| format!("在 {} 中找不到 {MANIFEST_NAME}", dir.display()))?;
                let sig = std::fs::read(dir.join(SIGNATURE_NAME)).ok();
                (bytes, sig, Locator::Dir(dir.clone()))
            }
            remote => {
                let url = remote.api_url(product, version).expect("remote source");
                let mut req = http.get(&url);
                if matches!(remote, Source::GitHub) {
                    req = req.header("Accept", "application/vnd.github+json");
                }
                let resp = req.send().with_context(|| format!("无法连接 {}", remote.label()))?;
                if resp.status() == reqwest::StatusCode::NOT_FOUND {
                    let repo = repo(remote, product);
                    match version {
                        Some(v) => bail!("{} {repo} 上没有版本 v{}", remote.label(), v.trim_start_matches('v')),
                        None => bail!("{} {repo} 上还没有正式发布的版本（预发布版本请指定版本号）", remote.label()),
                    }
                }
                let rel: ApiRelease = resp.error_for_status()?.json().context("无法解析发布信息")?;
                let urls: HashMap<String, String> = rel.assets.into_iter().map(|a| (a.name, a.browser_download_url)).collect();
                let murl = urls.get(MANIFEST_NAME).with_context(|| {
                    format!("{} 发布 {} 中没有 {MANIFEST_NAME}，它不是 3.1.x 的发布（或发布不完整）", product.label(), rel.tag_name)
                })?;
                let bytes = get_small(http, murl)?;
                let sig = match urls.get(SIGNATURE_NAME) {
                    Some(u) => Some(get_small(http, u)?),
                    None => None,
                };
                (bytes, sig, Locator::Remote(urls))
            }
        };
        let (manifest, trust) = manifest::verify(product, &manifest_bytes, signature.as_deref(), policy)?;
        if let Some(v) = version {
            let want = manifest::Version::parse(v)?;
            let got = manifest::Version::parse(&manifest.version)?;
            if want != got {
                bail!("请求的 {} 版本为 {want}，但发布清单的版本为 {got}", product.label());
            }
        }
        Ok(Release { manifest, trust, manifest_bytes, signature, locator })
    }

    /// Write the verified manifest (and signature) into `dir`, so the
    /// installed version can later be checked and repaired offline.
    pub fn save_manifest(&self, dir: &Path) -> Result<()> {
        let (m, s) = saved_names(self.manifest.product);
        fsutil::atomic_write(&dir.join(m), &self.manifest_bytes, 0o644)?;
        if let Some(sig) = &self.signature {
            fsutil::atomic_write(&dir.join(s), sig, 0o644)?;
        }
        Ok(())
    }

    /// Download one artifact into `dir` and verify it.  Returns its path.
    pub fn fetch(&self, http: &reqwest::blocking::Client, artifact: &Artifact, dir: &Path) -> Result<PathBuf> {
        let dest = dir.join(&artifact.name);
        match &self.locator {
            Locator::Dir(src) => {
                let f = std::fs::File::open(src.join(&artifact.name))
                    .with_context(|| format!("在 {} 中找不到 {}", src.display(), artifact.name))?;
                stream_verified(f, artifact, &dest, false)?;
            }
            Locator::Remote(urls) => {
                let url = urls.get(&artifact.name).with_context(|| format!("发布中缺少文件 {}", artifact.name))?;
                retry(&artifact.name, || {
                    let resp = http.get(url).send()?.error_for_status()?;
                    stream_verified(resp, artifact, &dest, true)
                })?;
            }
        }
        Ok(dest)
    }
}

/// Subdirectory of a `--from-dir` directory.
pub fn dir_name(product: Product) -> &'static str {
    match product {
        Product::Core => "core",
        Product::Controller => "controller",
    }
}

/// File names of a product's manifest and signature saved in a version
/// directory.
pub fn saved_names(product: Product) -> (String, String) {
    let p = dir_name(product);
    (format!("{p}.{MANIFEST_NAME}"), format!("{p}.{SIGNATURE_NAME}"))
}

fn get_small(http: &reqwest::blocking::Client, url: &str) -> Result<Vec<u8>> {
    retry(url, || {
        let resp = http.get(url).send()?.error_for_status()?;
        let mut buf = Vec::new();
        resp.take(1024 * 1024 + 1).read_to_end(&mut buf)?;
        if buf.len() > 1024 * 1024 {
            bail!("{url} 过大");
        }
        Ok(buf)
    })
}

fn retry<T>(what: &str, mut f: impl FnMut() -> Result<T>) -> Result<T> {
    let mut last = None;
    for attempt in 1..=RETRIES {
        match f() {
            Ok(v) => return Ok(v),
            Err(e) => {
                // A hash mismatch won't fix itself; don't hammer the server.
                if e.downcast_ref::<Mismatch>().is_some() {
                    return Err(e);
                }
                if attempt < RETRIES {
                    eprintln!("  下载 {what} 失败（第 {attempt} 次）：{e:#}，{} 秒后重试…", attempt * 2);
                    std::thread::sleep(Duration::from_secs(u64::from(attempt) * 2));
                }
                last = Some(e);
            }
        }
    }
    Err(last.unwrap().context(format!("下载 {what} 失败")))
}

/// A downloaded file that doesn't match the manifest.
#[derive(Debug)]
pub struct Mismatch(String);

impl std::fmt::Display for Mismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Mismatch {}

/// Copy `reader` to `<dest>.part`, enforcing the manifest's size and hash,
/// then rename it to `dest`.
fn stream_verified(mut reader: impl Read, artifact: &Artifact, dest: &Path, progress: bool) -> Result<()> {
    let part = dest.with_file_name(format!("{}.part", artifact.name));
    let result = (|| -> Result<()> {
        let mut file = std::fs::File::create(&part).with_context(|| format!("无法写入 {}", part.display()))?;
        let pb = progress.then(|| {
            let pb = indicatif::ProgressBar::new(artifact.size);
            pb.set_style(
                indicatif::ProgressStyle::default_bar()
                    .template("  [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({eta})")
                    .unwrap()
                    .progress_chars("=>-"),
            );
            pb
        });
        let mut hasher = Sha256::new();
        let mut total = 0u64;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            total += n as u64;
            if total > artifact.size {
                return Err(Mismatch(format!("{} 超出清单记录的大小 {} 字节", artifact.name, artifact.size)).into());
            }
            hasher.update(&buf[..n]);
            file.write_all(&buf[..n])?;
            if let Some(pb) = &pb {
                pb.inc(n as u64);
            }
        }
        if let Some(pb) = pb {
            pb.finish_and_clear();
        }
        if total != artifact.size {
            bail!("{} 不完整：收到 {} 字节，应为 {} 字节", artifact.name, total, artifact.size);
        }
        let got = hex::encode(hasher.finalize());
        if !got.eq_ignore_ascii_case(&artifact.sha256) {
            return Err(Mismatch(format!("{} 的 SHA-256 校验失败（得到 {got}，应为 {}）", artifact.name, artifact.sha256)).into());
        }
        file.sync_all()?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            std::fs::rename(&part, dest)?;
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::remove_file(&part);
            Err(e)
        }
    }
}

/// Check a file already on disk against its manifest entry.
pub fn matches(path: &Path, artifact: &Artifact) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.len() == artifact.size)
        && fsutil::sha256_file(path).is_ok_and(|h| h.eq_ignore_ascii_case(&artifact.sha256))
}

/// Wintun from wintun.net, for releases that don't ship `wintun-*.dll`.
/// The zip's hash is pinned, so this is as safe as a manifest entry.
pub mod wintun {
    use super::*;
    use crate::platform::Arch;

    pub const URL: &str = "https://www.wintun.net/builds/wintun-0.14.1.zip";
    pub const SHA256: &str = "07c256185d6ee3652e09fa55c0b673e2624b565e02c4b9091c79ca7d2f24ef51";

    pub fn fetch(http: &reqwest::blocking::Client, arch: Arch, dest: &Path) -> Result<()> {
        let bytes = retry("wintun", || {
            let mut buf = Vec::new();
            http.get(URL).send()?.error_for_status()?.take(16 * 1024 * 1024).read_to_end(&mut buf)?;
            let got = hex::encode(Sha256::digest(&buf));
            if got != SHA256 {
                return Err(Mismatch(format!("wintun 压缩包校验失败（得到 {got}）")).into());
            }
            Ok(buf)
        })?;
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes))?;
        let inner = format!("wintun/bin/{}/wintun.dll", arch.wintun_dir());
        let mut entry = zip.by_name(&inner).with_context(|| format!("压缩包中没有 {inner}"))?;
        let mut out = std::fs::File::create(dest)?;
        std::io::copy(&mut entry, &mut out)?;
        out.sync_all()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::tests::sample;

    #[test]
    fn verified_download_rejects_bad_content() {
        let d = tempfile::tempdir().unwrap();
        let art = &sample("3.1.0").artifacts[0]; // "abc"
        let dest = d.path().join(&art.name);

        stream_verified(&b"abc"[..], art, &dest, false).unwrap();
        assert!(matches(&dest, art));

        for bad in [&b"abd"[..], &b"ab"[..], &b"abcd"[..]] {
            std::fs::remove_file(&dest).unwrap();
            assert!(stream_verified(bad, art, &dest, false).is_err());
            assert!(!dest.exists(), "nothing is left under the final name");
            assert_eq!(fsutil::list_dir(d.path()).len(), 0, "the partial file is removed");
            std::fs::write(&dest, b"abc").unwrap();
        }
    }

    #[test]
    fn local_dir_release() {
        let d = tempfile::tempdir().unwrap();
        let m = sample("3.1.0");
        let core = d.path().join("core");
        std::fs::create_dir_all(&core).unwrap();
        std::fs::write(core.join(MANIFEST_NAME), serde_json::to_vec(&m).unwrap()).unwrap();
        std::fs::write(core.join(&m.artifacts[0].name), b"abc").unwrap();
        let policy = TrustPolicy { core: vec![], controller: vec![], allow_unsigned: true };
        let http = http_client().unwrap();
        let src = Source::Dir(d.path().into());
        let rel = Release::resolve(&http, &src, Product::Core, Some("3.1.0"), &policy).unwrap();
        assert!(Release::resolve(&http, &src, Product::Core, Some("3.1.1"), &policy).is_err());
        assert!(Release::resolve(&http, &src, Product::Controller, None, &policy).is_err());
        let out = tempfile::tempdir().unwrap();
        let p = rel.fetch(&http, &rel.manifest.artifacts[0], out.path()).unwrap();
        assert_eq!(std::fs::read(p).unwrap(), b"abc");
        let strict = TrustPolicy { core: vec![], controller: vec![], allow_unsigned: false };
        assert!(Release::resolve(&http, &src, Product::Core, None, &strict).is_err());
    }
}
