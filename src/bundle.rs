//! A Core release plus (when a controller is installed) a Controller
//! release: the unit the native deployment installs, keeps and rolls back
//! (`versions/<core>+<controller>/`).  Core and the Controller are released
//! from separate repositories with independent versions.

use crate::context::Ctx;
use crate::manifest::{self, Product};
use crate::release::Release;
use anyhow::{Result, bail};
use std::cmp::Ordering;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Versions {
    pub core: String,
    pub controller: Option<String>,
}

impl Versions {
    /// The version directory name / state id: `<core>` or `<core>+<controller>`.
    pub fn id(&self) -> String {
        match &self.controller {
            Some(c) => format!("{}+{}", self.core, c),
            None => self.core.clone(),
        }
    }

    pub fn parse_id(id: &str) -> Versions {
        match id.split_once('+') {
            Some((core, ctl)) => Versions { core: core.into(), controller: Some(ctl.into()) },
            None => Versions { core: id.into(), controller: None },
        }
    }

    pub fn label(&self) -> String {
        match &self.controller {
            Some(c) => format!("Core {} / 控制器 {}", self.core, c),
            None => format!("Core {}", self.core),
        }
    }

    /// Whether any component goes back to an older version.
    pub fn is_downgrade_from(&self, old: &Versions) -> bool {
        let older = |new: &str, old: &str| manifest::compare(new, old) == Ordering::Less;
        older(&self.core, &old.core)
            || matches!((&self.controller, &old.controller), (Some(n), Some(o)) if older(n, o))
    }
}

pub struct Bundle {
    pub core: Release,
    pub controller: Option<Release>,
}

impl Bundle {
    /// Resolve the Core release and, if `with_controller`, the Controller
    /// release; check that they work together.
    pub fn resolve(ctx: &Ctx, core: Option<&str>, controller: Option<&str>, with_controller: bool) -> Result<Bundle> {
        let core = ctx.resolve(Product::Core, core)?;
        let controller = if with_controller { Some(ctx.resolve(Product::Controller, controller)?) } else { None };
        let b = Bundle { core, controller };
        b.check_compatible(ctx)?;
        Ok(b)
    }

    pub fn versions(&self) -> Versions {
        Versions { core: self.core.manifest.version.clone(), controller: self.controller.as_ref().map(|c| c.manifest.version.clone()) }
    }

    fn check_compatible(&self, ctx: &Ctx) -> Result<()> {
        let Some(ctl) = &self.controller else { return Ok(()) };
        let (core, ctl) = (&self.core.manifest, &ctl.manifest);
        if let Some(min) = &ctl.min_core
            && manifest::compare(&core.version, min) == Ordering::Less
        {
            bail!("控制器 {} 需要 Core {} 或更新，所选 Core 为 {}（可用 --core-version 指定）", ctl.version, min, core.version);
        }
        if let (Some(a), Some(b)) = (&core.ipc_protocol, &ctl.ipc_protocol)
            && a != b
        {
            ctx.ui.warn(&format!("Core 使用 IPC 协议 {a}，控制器基于协议 {b} 构建；部分功能可能不可用。"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids() {
        let v = Versions { core: "3.1.0-alpha.1".into(), controller: Some("3.1.0-alpha.2".into()) };
        assert_eq!(v.id(), "3.1.0-alpha.1+3.1.0-alpha.2");
        assert_eq!(Versions::parse_id(&v.id()), v);
        assert_eq!(Versions::parse_id("3.1.2"), Versions { core: "3.1.2".into(), controller: None });
        let older = Versions { core: "3.1.0".into(), controller: Some("3.1.0-alpha.2".into()) };
        let newer = Versions { core: "3.1.0".into(), controller: Some("3.1.0".into()) };
        assert!(older.is_downgrade_from(&newer));
        assert!(!newer.is_downgrade_from(&older));
        assert!(!Versions { core: "3.1.1".into(), controller: None }.is_downgrade_from(&newer));
    }
}
