//! System services: definitions and lifecycle.
//!
//! | | Linux (systemd) | macOS (launchd) | Windows (SCM) |
//! |---|---|---|---|
//! | Core | `msst-net.service`, root | `net.msst.client`, root | `msst-net`, LocalSystem |
//! | WebUI | `msst-net-webui.service`, user `msst-webui` | `net.msst.webui`, root | `msst-net-webui`, LocalSystem, after `msst-net` |
//!
//! Names are the 3.0.x ones, so a migrated system has one Core service, not
//! two (Core's Windows service host also registers as `msst-net`).
//! Definitions point at a versioned executable; switching versions =
//! rewriting the definition, which makes rollback a definition rewrite too.

use crate::accounts::{ADMIN_GROUP_UNIX, WEBUI_USER};
use crate::cmd;
use crate::fsutil;
use crate::platform::Os;
use anyhow::Result;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Service {
    Core,
    Webui,
}

const SYSTEMD_DIR: &str = "/etc/systemd/system";
const LAUNCHD_DIR: &str = "/Library/LaunchDaemons";
const HEADER: &str = "# Managed by msst-net-installer: upgrade/repair/rollback rewrite this file.\n\
                      # Put local changes in a drop-in (systemctl edit <unit>).\n";

impl Service {
    pub fn name(self, os: Os) -> &'static str {
        match (self, os) {
            (Service::Core, Os::Macos) => "net.msst.client",
            (Service::Webui, Os::Macos) => "net.msst.webui",
            (Service::Core, _) => "msst-net",
            (Service::Webui, _) => "msst-net-webui",
        }
    }

    /// The definition file (systemd unit / launchd plist).
    pub fn definition_path(self, os: Os) -> Option<PathBuf> {
        match os {
            Os::Linux => Some(PathBuf::from(SYSTEMD_DIR).join(format!("{}.service", self.name(os)))),
            Os::Macos => Some(PathBuf::from(LAUNCHD_DIR).join(format!("{}.plist", self.name(os)))),
            Os::Windows => None,
        }
    }
}

/// What a service runs.
pub struct Definition {
    pub exe: PathBuf,
    /// HOME of the service account (pins Core's data-dir resolution).
    pub home: PathBuf,
    /// WebUI: config directory and an optional first-start password file.
    pub webui_config: Option<PathBuf>,
    pub password_file: Option<PathBuf>,
}

pub fn render_systemd(svc: Service, def: &Definition) -> String {
    match svc {
        Service::Core => format!(
            "{HEADER}[Unit]\n\
             Description=MSST-Net Core\n\
             After=network-online.target\n\
             Wants=network-online.target\n\
             StartLimitIntervalSec=300\n\
             StartLimitBurst=5\n\
             \n\
             [Service]\n\
             Type=simple\n\
             ExecStart={exe} --foreground\n\
             Environment=HOME={home}\n\
             Restart=on-failure\n\
             RestartSec=5\n\
             TimeoutStopSec=20\n\
             LimitNOFILE=65536\n\
             # Core manages TUN, routes and DNS as root; it never needs to gain more.\n\
             NoNewPrivileges=yes\n\
             PrivateTmp=yes\n\
             \n\
             [Install]\n\
             WantedBy=multi-user.target\n",
            exe = def.exe.display(),
            home = def.home.display(),
        ),
        Service::Webui => {
            let config = def.webui_config.as_deref().expect("webui config dir");
            let password = def
                .password_file
                .as_ref()
                .map(|p| format!("Environment=PASSWORD_FILE={}\n", p.display()))
                .unwrap_or_default();
            format!(
                "{HEADER}[Unit]\n\
                 Description=MSST-Net WebUI Controller\n\
                 After=network-online.target msst-net.service\n\
                 Wants=network-online.target\n\
                 \n\
                 [Service]\n\
                 Type=simple\n\
                 # Unprivileged; admin access to Core comes from the {group} group.\n\
                 User={user}\n\
                 Group={group}\n\
                 ExecStart={exe}\n\
                 Environment=MSST_WEBUI_CONFIG_DIR={config}\n\
                 {password}\
                 Restart=on-failure\n\
                 RestartSec=5\n\
                 NoNewPrivileges=yes\n\
                 ProtectSystem=full\n\
                 ProtectHome=yes\n\
                 PrivateTmp=yes\n\
                 PrivateDevices=yes\n\
                 ProtectKernelTunables=yes\n\
                 ProtectKernelModules=yes\n\
                 ProtectControlGroups=yes\n\
                 RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6\n\
                 LockPersonality=yes\n\
                 \n\
                 [Install]\n\
                 WantedBy=multi-user.target\n",
                exe = def.exe.display(),
                config = config.display(),
                user = WEBUI_USER,
                group = ADMIN_GROUP_UNIX,
            )
        }
    }
}

pub fn render_launchd(svc: Service, def: &Definition) -> String {
    let mut args = vec![xml_escape(&def.exe.to_string_lossy())];
    if svc == Service::Core {
        args.push("--foreground".into());
    }
    let mut env = vec![("HOME".to_string(), def.home.to_string_lossy().into_owned())];
    env.extend(environment(def));
    let args: String = args.iter().map(|a| format!("        <string>{a}</string>\n")).collect();
    let env: String = env
        .iter()
        .map(|(k, v)| format!("        <key>{}</key>\n        <string>{}</string>\n", k, xml_escape(v)))
        .collect();
    let name = svc.name(Os::Macos);
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <!-- Managed by msst-net-installer: upgrade/repair/rollback rewrite this file. -->\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \x20   <key>Label</key>\n\
         \x20   <string>{name}</string>\n\
         \x20   <key>ProgramArguments</key>\n\
         \x20   <array>\n{args}    </array>\n\
         \x20   <key>EnvironmentVariables</key>\n\
         \x20   <dict>\n{env}    </dict>\n\
         \x20   <key>RunAtLoad</key>\n\
         \x20   <true/>\n\
         \x20   <key>KeepAlive</key>\n\
         \x20   <dict>\n\
         \x20       <key>SuccessfulExit</key>\n\
         \x20       <false/>\n\
         \x20   </dict>\n\
         \x20   <key>ThrottleInterval</key>\n\
         \x20   <integer>5</integer>\n\
         \x20   <key>StandardErrorPath</key>\n\
         \x20   <string>/var/log/{name}.log</string>\n\
         </dict>\n\
         </plist>\n"
    )
}

/// Environment variables a service definition passes to the program.
fn environment(def: &Definition) -> Vec<(String, String)> {
    let mut env = Vec::new();
    if let Some(c) = &def.webui_config {
        env.push(("MSST_WEBUI_CONFIG_DIR".into(), c.to_string_lossy().into_owned()));
    }
    if let Some(p) = &def.password_file {
        env.push(("PASSWORD_FILE".into(), p.to_string_lossy().into_owned()));
    }
    env
}

/// The SCM passes a service the `Environment` value (REG_MULTI_SZ) of its
/// registry key.
fn set_windows_environment(name: &str, env: &[(String, String)]) -> Result<()> {
    let key = format!(r"HKLM\SYSTEM\CurrentControlSet\Services\{name}");
    if env.is_empty() {
        let _ = cmd::run_quiet("reg", &["delete", &key, "/v", "Environment", "/f"]);
        return Ok(());
    }
    // '|' separates the strings: it can't occur in a Windows path, while
    // reg.exe's default "\0" could clash with a path like C:\0data.
    cmd::run_quiet("reg", &["add", &key, "/v", "Environment", "/t", "REG_MULTI_SZ", "/s", "|", "/d", &multi_sz(env), "/f"])
}

fn multi_sz(env: &[(String, String)]) -> String {
    env.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("|")
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Write (or rewrite) a service definition and enable it at boot.  Does not
/// start it.
pub fn install(os: Os, svc: Service, def: &Definition) -> Result<()> {
    match os {
        Os::Linux => {
            fsutil::atomic_write(&svc.definition_path(os).unwrap(), render_systemd(svc, def).as_bytes(), 0o644)?;
            cmd::run_quiet("systemctl", &["daemon-reload"])?;
            cmd::run_quiet("systemctl", &["enable", svc.name(os)])?;
        }
        Os::Macos => {
            let path = svc.definition_path(os).unwrap();
            // A loaded job keeps its old definition until unloaded.
            let _ = cmd::run_quiet("launchctl", &["unload", &path.to_string_lossy()]);
            fsutil::atomic_write(&path, render_launchd(svc, def).as_bytes(), 0o644)?;
        }
        Os::Windows => {
            // Quoted: the path contains spaces ("Program Files"); an unquoted
            // service path is a privilege-escalation hole.
            let bin = format!("\"{}\"", def.exe.display());
            let name = svc.name(os);
            let (display, description) = match svc {
                Service::Core => ("MSST-Net Core", "MSST-Net Core 网络服务"),
                Service::Webui => ("MSST-Net WebUI", "MSST-Net WebUI 控制器（HTTPS）"),
            };
            let mut args = vec![if exists(os, svc) { "config" } else { "create" }, name, "binPath=", &bin, "start=", "auto"];
            if svc == Service::Webui {
                // Starts after Core; runs as LocalSystem (the default), as
                // Core's pipe requires.
                args.extend(["depend=", Service::Core.name(os)]);
            }
            if args[0] == "create" {
                args.extend(["DisplayName=", display]);
            }
            cmd::run_quiet("sc", &args)?;
            cmd::run_quiet("sc", &["description", name, description])?;
            // Recovery: restart after a crash (5 s, 10 s, 30 s; counter reset daily).
            cmd::run_quiet("sc", &["failure", name, "reset=", "86400", "actions=", "restart/5000/restart/10000/restart/30000"])?;
            set_windows_environment(name, &environment(def))?;
        }
    }
    Ok(())
}

pub fn exists(os: Os, svc: Service) -> bool {
    match os {
        Os::Windows => cmd::succeeds("sc", &["query", svc.name(os)]),
        _ => svc.definition_path(os).is_some_and(|p| p.exists()),
    }
}

pub fn start(os: Os, svc: Service) -> Result<()> {
    match os {
        Os::Linux => cmd::run_quiet("systemctl", &["start", svc.name(os)]),
        Os::Macos => cmd::run_quiet("launchctl", &["load", "-w", &svc.definition_path(os).unwrap().to_string_lossy()]),
        Os::Windows => cmd::run_quiet("sc", &["start", svc.name(os)]),
    }
}

/// Stop a service; a service that isn't running or doesn't exist is fine.
pub fn stop(os: Os, svc: Service) {
    match os {
        Os::Linux => {
            let _ = cmd::run_quiet("systemctl", &["stop", svc.name(os)]);
        }
        Os::Macos => {
            if let Some(p) = svc.definition_path(os) {
                let _ = cmd::run_quiet("launchctl", &["unload", &p.to_string_lossy()]);
            }
        }
        Os::Windows => {
            let _ = cmd::run_quiet("sc", &["stop", svc.name(os)]);
            wait_windows_stopped(svc.name(os));
        }
    }
}

/// `sc stop` returns before the service has stopped; replacing its files
/// before then fails (the executable is locked).
fn wait_windows_stopped(name: &str) {
    for _ in 0..30 {
        match cmd::output("sc", &["query", name]) {
            Ok(out) if out.contains("RUNNING") || out.contains("STOP_PENDING") => {
                std::thread::sleep(std::time::Duration::from_secs(1))
            }
            _ => return,
        }
    }
}

pub fn is_running(os: Os, svc: Service) -> bool {
    match os {
        Os::Linux => cmd::succeeds("systemctl", &["is-active", "--quiet", svc.name(os)]),
        Os::Macos => cmd::output("launchctl", &["list", svc.name(os)]).is_ok_and(|o| o.contains("\"PID\"")),
        Os::Windows => cmd::output("sc", &["query", svc.name(os)]).is_ok_and(|o| o.contains("RUNNING")),
    }
}

/// Stop, disable and delete a service.
pub fn remove(os: Os, svc: Service) -> Result<()> {
    stop(os, svc);
    match os {
        Os::Linux => {
            let _ = cmd::run_quiet("systemctl", &["disable", svc.name(os)]);
            fsutil::remove_any(&svc.definition_path(os).unwrap())?;
            let _ = cmd::run_quiet("systemctl", &["daemon-reload"]);
            let _ = cmd::run_quiet("systemctl", &["reset-failed", svc.name(os)]);
        }
        Os::Macos => fsutil::remove_any(&svc.definition_path(os).unwrap())?,
        Os::Windows => {
            if exists(os, svc) {
                cmd::run_quiet("sc", &["delete", svc.name(os)])?;
            }
        }
    }
    Ok(())
}

/// The executable an installed service definition points at (for
/// detecting what a 3.0.x service runs).
pub fn current_exe(os: Os, svc: Service) -> Option<String> {
    match os {
        Os::Windows => {
            let out = cmd::output("sc", &["qc", svc.name(os)]).ok()?;
            out.lines().find_map(|l| l.trim().strip_prefix("BINARY_PATH_NAME").map(|v| v.trim_start_matches([' ', ':']).trim().to_string()))
        }
        _ => {
            let text = std::fs::read_to_string(svc.definition_path(os)?).ok()?;
            match os {
                Os::Linux => text.lines().find_map(|l| l.strip_prefix("ExecStart=").map(|v| v.trim().to_string())),
                _ => text.split("<string>").nth(2).and_then(|s| s.split("</string>").next()).map(str::to_string),
            }
        }
    }
}

/// Recent log lines of a service, for failure reports.
pub fn recent_logs(os: Os, svc: Service) -> String {
    match os {
        Os::Linux => cmd::output("journalctl", &["-u", svc.name(os), "-n", "20", "--no-pager", "-o", "cat"]).unwrap_or_default(),
        Os::Macos => std::fs::read_to_string(format!("/var/log/{}.log", svc.name(os)))
            .map(|t| t.lines().rev().take(20).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n"))
            .unwrap_or_default(),
        // The WebUI service logs to its config directory; Core logs nowhere
        // the installer can read.
        Os::Windows if svc == Service::Webui => std::fs::read_to_string(crate::layout::webui_config_dir(os).join("webui.log"))
            .map(|t| t.lines().rev().take(20).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n"))
            .unwrap_or_default(),
        Os::Windows => String::new(),
    }
}

pub fn definition_for_core(exe: &Path, home: &Path) -> Definition {
    Definition { exe: exe.into(), home: home.into(), webui_config: None, password_file: None }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn systemd_units() {
        let core = render_systemd(Service::Core, &definition_for_core(Path::new("/opt/msst-net/versions/3.1.0/msst-net-core"), Path::new("/root")));
        assert!(core.contains("ExecStart=/opt/msst-net/versions/3.1.0/msst-net-core --foreground\n"));
        assert!(core.contains("Environment=HOME=/root\n"));
        assert!(!core.contains("User="), "Core runs as root");

        let def = Definition {
            exe: "/opt/msst-net/versions/3.1.0/msst-net-webui".into(),
            home: "/var/lib/msst-net-webui".into(),
            webui_config: Some("/var/lib/msst-net-webui".into()),
            password_file: Some("/var/lib/msst-net-webui/initial-password".into()),
        };
        let webui = render_systemd(Service::Webui, &def);
        assert!(webui.contains("User=msst-webui\nGroup=msst-net\n"));
        assert!(webui.contains("Environment=PASSWORD_FILE=/var/lib/msst-net-webui/initial-password\n"));
        assert!(webui.contains("NoNewPrivileges=yes"));
        let webui = render_systemd(Service::Webui, &Definition { password_file: None, ..def });
        assert!(!webui.contains("PASSWORD_FILE"));
    }

    #[test]
    fn windows_service_environment() {
        let def = Definition {
            exe: r"C:\Program Files\MSST-Net\versions\3.1.0+3.1.0\msst-net-webui.exe".into(),
            home: r"C:\ProgramData\MSST-Net\webui".into(),
            webui_config: Some(r"C:\ProgramData\MSST-Net\webui".into()),
            password_file: Some(r"C:\ProgramData\MSST-Net\webui\initial-password".into()),
        };
        assert_eq!(
            multi_sz(&environment(&def)),
            r"MSST_WEBUI_CONFIG_DIR=C:\ProgramData\MSST-Net\webui|PASSWORD_FILE=C:\ProgramData\MSST-Net\webui\initial-password"
        );
        assert!(environment(&definition_for_core(Path::new("x"), Path::new("y"))).is_empty(), "Core gets no extra environment");
    }

    #[test]
    fn launchd_plist() {
        let p = render_launchd(Service::Core, &definition_for_core(Path::new("/usr/local/msst-net/versions/3.1.0/msst-net-core"), Path::new("/var/root")));
        assert!(p.contains("<string>net.msst.client</string>"));
        assert!(p.contains("<string>/usr/local/msst-net/versions/3.1.0/msst-net-core</string>\n        <string>--foreground</string>"));
        assert!(p.contains("<key>HOME</key>\n        <string>/var/root</string>"));
    }
}
