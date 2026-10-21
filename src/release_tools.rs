//! `msst-net-installer release …`: the release pipeline's side of the
//! contract — generate the manifest from a directory of built artifacts,
//! sign it, verify it.  Keeping this in the installer means the producer
//! and the consumer of the manifest can't drift apart.

use crate::fsutil;
use crate::manifest::{self, Artifact, MANIFEST_NAME, Manifest, Product, SIGNATURE_NAME, TrustPolicy};
use anyhow::{Context, Result, bail};
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use std::path::Path;

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// Create a signing key (secret file, mode 0600) and print the public key
/// to build installers with (`MSST_CORE_RELEASE_PUBKEYS` /
/// `MSST_CONTROLLER_RELEASE_PUBKEYS`, one key per repository).
pub fn keygen(out: &Path) -> Result<()> {
    if out.exists() {
        bail!("{} 已存在，不会覆盖", out.display());
    }
    let key = SigningKey::generate(&mut rand::rngs::OsRng);
    fsutil::atomic_write(out, B64.encode(key.to_bytes()).as_bytes(), 0o600)?;
    println!("私钥已写入 {}（妥善保管，仅放在 CI 机密中）", out.display());
    println!("公钥（构建安装器时设置 MSST_CORE_RELEASE_PUBKEYS 或 MSST_CONTROLLER_RELEASE_PUBKEYS）：");
    println!("{}", B64.encode(key.verifying_key().to_bytes()));
    Ok(())
}

/// Optional manifest fields.
#[derive(Default)]
pub struct Extra {
    pub ipc_protocol: Option<String>,
    pub image: Option<String>,
    pub min_core: Option<String>,
    pub min_installer: Option<String>,
}

/// Write `manifest.json` for `product`'s artifacts in `dir`.  Files that
/// don't follow the naming contract, or belong to the other product, are
/// reported and left out.
pub fn generate(product: Product, dir: &Path, version: &str, extra: Extra) -> Result<()> {
    let mut artifacts = Vec::new();
    for path in fsutil::list_dir(dir) {
        let name = fsutil::file_name(&path);
        if !path.is_file() || name == MANIFEST_NAME || name == SIGNATURE_NAME {
            continue;
        }
        let Some((component, os, arch, format)) = manifest::parse_asset_name(&name) else {
            eprintln!("跳过（不符合命名约定）：{name}");
            continue;
        };
        if !product.owns(component) {
            eprintln!("跳过（不属于 {}）：{name}", product.label());
            continue;
        }
        let size = std::fs::metadata(&path)?.len();
        artifacts.push(Artifact { name, component, os, arch, format, size, sha256: fsutil::sha256_file(&path)? });
    }
    if artifacts.is_empty() {
        bail!("{} 中没有符合命名约定的文件", dir.display());
    }
    if extra.min_core.is_some() && product != Product::Controller {
        bail!("--min-core 只用于控制器的发布");
    }
    let m = Manifest {
        schema: manifest::SCHEMA,
        product,
        version: manifest::Version::parse(version)?.to_string(),
        ipc_protocol: extra.ipc_protocol,
        min_core: extra.min_core,
        min_installer: extra.min_installer,
        artifacts,
        image: extra.image,
    };
    m.validate()?;
    let mut bytes = serde_json::to_vec_pretty(&m)?;
    bytes.push(b'\n');
    fsutil::atomic_write(&dir.join(MANIFEST_NAME), &bytes, 0o644)?;
    println!("已写入 {}（{} 个文件）", dir.join(MANIFEST_NAME).display(), m.artifacts.len());
    Ok(())
}

/// Sign `<dir>/manifest.json` → `<dir>/manifest.json.sig`.  The key comes
/// from a file or, in CI, the `MSST_RELEASE_SIGNING_KEY` variable.
pub fn sign(dir: &Path, key_file: Option<&Path>) -> Result<()> {
    let encoded = match key_file {
        Some(p) => std::fs::read_to_string(p).with_context(|| format!("无法读取 {}", p.display()))?,
        None => std::env::var("MSST_RELEASE_SIGNING_KEY").context("请用 --key 或 MSST_RELEASE_SIGNING_KEY 提供私钥")?,
    };
    let raw = B64.decode(encoded.trim()).context("私钥不是有效的 base64")?;
    let key = SigningKey::from_bytes(&raw.try_into().map_err(|_| anyhow::anyhow!("私钥长度应为 32 字节"))?);
    let bytes = std::fs::read(dir.join(MANIFEST_NAME))?;
    // Never sign something the installer would reject.
    let _: Manifest = serde_json::from_slice(&bytes).context("清单无法解析")?;
    let sig = B64.encode(key.sign(&bytes).to_bytes());
    fsutil::atomic_write(&dir.join(SIGNATURE_NAME), format!("{sig}\n").as_bytes(), 0o644)?;
    println!("已签名；公钥 {}", B64.encode(key.verifying_key().to_bytes()));
    Ok(())
}

/// Verify a release directory exactly as the installer will.
pub fn verify(product: Product, dir: &Path, pubkeys: &[String], allow_unsigned: bool) -> Result<()> {
    let mut policy = TrustPolicy::builtin(allow_unsigned)?;
    for k in pubkeys {
        policy.keys_mut(product).push(manifest::decode_public_key(k)?);
    }
    let bytes = std::fs::read(dir.join(MANIFEST_NAME))?;
    let sig = std::fs::read(dir.join(SIGNATURE_NAME)).ok();
    let (m, trust) = manifest::verify(product, &bytes, sig.as_deref(), &policy)?;
    println!("{} 清单 {}：{:?}，镜像 {}", product.label(), m.version, trust, m.image());
    let mut bad = 0;
    for a in &m.artifacts {
        let ok = crate::release::matches(&dir.join(&a.name), a);
        println!("  {} {}", if ok { "✓" } else { "✗" }, a.name);
        bad += usize::from(!ok);
    }
    if bad > 0 {
        bail!("{bad} 个文件与清单不符");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_sign_verify() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("msst-net-core-linux-x86_64"), b"core").unwrap();
        std::fs::write(d.path().join("msst-net-controller-desktop-linux-x86_64.deb"), b"deb").unwrap();
        std::fs::write(d.path().join("README.txt"), b"ignored").unwrap();
        generate(Product::Core, d.path(), "v3.1.0", Extra { ipc_protocol: Some("0.4".into()), ..Extra::default() }).unwrap();
        let m: Manifest = serde_json::from_slice(&std::fs::read(d.path().join(MANIFEST_NAME)).unwrap()).unwrap();
        assert_eq!(m.version, "3.1.0");
        assert_eq!(m.artifacts.len(), 1, "the desktop package belongs to the Controller release");

        let key = SigningKey::from_bytes(&[3u8; 32]);
        let keyfile = d.path().join("key");
        std::fs::write(&keyfile, B64.encode(key.to_bytes())).unwrap();
        sign(d.path(), Some(&keyfile)).unwrap();
        let pubkey = B64.encode(key.verifying_key().to_bytes());
        verify(Product::Core, d.path(), std::slice::from_ref(&pubkey), false).unwrap();
        assert!(verify(Product::Controller, d.path(), std::slice::from_ref(&pubkey), false).is_err());

        std::fs::write(d.path().join("msst-net-core-linux-x86_64"), b"evil").unwrap();
        assert!(verify(Product::Core, d.path(), &[pubkey], false).is_err(), "a changed artifact fails");
    }
}
