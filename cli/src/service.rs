//! Background service: launchd (macOS) or systemd --user (Linux), restarted when it exits.

use anyhow::{bail, Result};
use std::path::PathBuf;
use std::process::Command;

const LABEL: &str = "talk.letagents.daemon";

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").expect("HOME"))
}

pub fn run(action: &str, target: &str) -> Result<()> {
    let exe = std::env::current_exe()?;
    let log = crate::store::home().join("daemon.log");
    if cfg!(target_os = "macos") {
        let plist = home().join(format!("Library/LaunchAgents/{LABEL}.plist"));
        let domain = format!("gui/{}", unsafe_uid());
        match action {
            "install" => {
                std::fs::create_dir_all(plist.parent().unwrap())?;
                std::fs::write(
                    &plist,
                    format!(
                        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key><array><string>{}</string><string>daemon</string><string>--target</string><string>{target}</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>10</integer>
  <key>StandardOutPath</key><string>{}</string>
  <key>StandardErrorPath</key><string>{}</string>
</dict></plist>
"#,
                        exe.display(),
                        log.display(),
                        log.display()
                    ),
                )?;
                let _ = Command::new("launchctl").args(["bootout", &format!("{domain}/{LABEL}")]).status();
                Command::new("launchctl").args(["bootstrap", &domain, &plist.to_string_lossy()]).status()?;
                println!("installed {LABEL}; log: {}", log.display());
            }
            "uninstall" => {
                let _ = Command::new("launchctl").args(["bootout", &format!("{domain}/{LABEL}")]).status();
                let _ = std::fs::remove_file(&plist);
                println!("removed {LABEL}");
            }
            _ => {
                Command::new("launchctl").args(["print", &format!("{domain}/{LABEL}")]).status()?;
            }
        }
    } else if cfg!(target_os = "linux") {
        let unit = home().join(".config/systemd/user/letagents.service");
        match action {
            "install" => {
                std::fs::create_dir_all(unit.parent().unwrap())?;
                std::fs::write(
                    &unit,
                    format!(
                        "[Unit]\nDescription=letagents daemon\n\n[Service]\nExecStart={} daemon --target {target}\nRestart=always\nRestartSec=10\n\n[Install]\nWantedBy=default.target\n",
                        exe.display()
                    ),
                )?;
                Command::new("systemctl").args(["--user", "daemon-reload"]).status()?;
                Command::new("systemctl").args(["--user", "enable", "--now", "letagents"]).status()?;
                println!("installed letagents.service; logs: journalctl --user -u letagents");
            }
            "uninstall" => {
                let _ = Command::new("systemctl").args(["--user", "disable", "--now", "letagents"]).status();
                let _ = std::fs::remove_file(&unit);
                println!("removed letagents.service");
            }
            _ => {
                Command::new("systemctl").args(["--user", "status", "letagents", "--no-pager"]).status()?;
            }
        }
    } else {
        bail!("service install supports macOS and Linux; run `letagents daemon` yourself elsewhere");
    }
    Ok(())
}

fn unsafe_uid() -> String {
    String::from_utf8(Command::new("id").arg("-u").output().map(|o| o.stdout).unwrap_or_default()).unwrap_or_default().trim().to_string()
}
