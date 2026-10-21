//! Everything an operation needs to know about this run.

use crate::layout::Layout;
use crate::manifest::{Artifact, Product, TrustPolicy};
use crate::platform::{Arch, Os};
use crate::release::{Release, Source};
use crate::ui::Ui;
use anyhow::Result;
use std::path::PathBuf;

pub struct Ctx {
    pub os: Os,
    pub arch: Arch,
    pub layout: Layout,
    pub ui: Ui,
    pub http: reqwest::blocking::Client,
    pub trust: TrustPolicy,
    pub source: Source,
    pub dry_run: bool,
    /// HOME of the account Core runs as.
    pub home: PathBuf,
}

impl Ctx {
    pub fn resolve(&self, product: Product, version: Option<&str>) -> Result<Release> {
        self.ui.section(&format!("从 {} 获取{}发布信息", self.source.label(), product.label()));
        let rel = Release::resolve(&self.http, &self.source, product, version, &self.trust)?;
        let what = format!("{} {}", product.label(), rel.manifest.version);
        match rel.trust {
            crate::manifest::Trust::Signed => self.ui.success(&format!("{what}：清单签名验证通过")),
            crate::manifest::Trust::Unsigned => self.ui.warn(&format!("{what}：清单未经签名验证（--allow-unsigned），仅校验了文件哈希")),
        }
        Ok(rel)
    }

    /// A new, empty staging directory.
    pub fn staging(&self) -> Result<PathBuf> {
        let dir = self.layout.staging_dir().join(crate::fsutil::stamp());
        std::fs::create_dir_all(&dir)?;
        crate::fsutil::set_mode(&dir, 0o700)?;
        Ok(dir)
    }

    /// Download and verify `artifacts` into `dir`.
    pub fn download(&self, release: &Release, artifacts: &[&Artifact], dir: &std::path::Path) -> Result<()> {
        for a in artifacts {
            self.ui.step(&format!("下载 {}（{}）", a.name, crate::fsutil::human_size(a.size)));
            release.fetch(&self.http, a, dir)?;
        }
        if !artifacts.is_empty() {
            self.ui.success("全部文件已下载并通过 SHA-256 校验");
        }
        Ok(())
    }
}
