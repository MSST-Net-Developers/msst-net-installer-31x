//! Terminal output and prompts.  In unattended mode (`--yes` /
//! `--non-interactive`) prompts never block: confirmations take `--yes`,
//! anything else that would need an answer is an error naming the flag.

use anyhow::{Result, bail};
use console::style;
use dialoguer::{Confirm, Input, Password, Select, theme::ColorfulTheme};

#[derive(Clone, Copy)]
pub struct Ui {
    /// Prompts may be shown.
    pub interactive: bool,
    /// Confirmations are answered "yes".
    pub assume_yes: bool,
}

impl Ui {
    pub fn banner(&self) {
        println!();
        println!("  {}", style("╭──────────────────────────────╮").cyan());
        println!(
            "  {}  {}  {}",
            style("│").cyan(),
            style(format!("MSST-Net 3.1 安装程序 {:<6}", short_version())).cyan().bold(),
            style("│").cyan()
        );
        println!("  {}", style("╰──────────────────────────────╯").cyan());
        println!();
    }

    pub fn section(&self, title: &str) {
        println!();
        println!("  {} {}", style("──").dim(), style(title).bold());
        println!();
    }

    pub fn success(&self, msg: &str) {
        println!("  {} {}", style("✓").green().bold(), style(msg).bold());
    }

    pub fn info(&self, msg: &str) {
        println!("  {} {}", style("·").dim(), msg);
    }

    pub fn warn(&self, msg: &str) {
        println!("  {} {}", style("⚠").yellow().bold(), style(msg).yellow());
    }

    pub fn step(&self, msg: &str) {
        println!("  {} {}", style("→").cyan(), msg);
    }

    /// Ask for confirmation.  Unattended: `--yes` answers yes, otherwise it
    /// is an error (an unattended run must not silently skip a step).
    pub fn confirm(&self, question: &str, default: bool) -> Result<bool> {
        if self.assume_yes {
            return Ok(true);
        }
        if !self.interactive {
            bail!("需要确认：{question}（无人值守模式请加 --yes）");
        }
        Ok(Confirm::with_theme(&ColorfulTheme::default()).with_prompt(question).default(default).interact()?)
    }

    /// A yes/no choice that has a sensible unattended answer (`default`).
    pub fn choose_yn(&self, question: &str, default: bool) -> Result<bool> {
        if !self.interactive {
            return Ok(default);
        }
        Ok(Confirm::with_theme(&ColorfulTheme::default()).with_prompt(question).default(default).interact()?)
    }

    pub fn select(&self, prompt: &str, items: &[&str], default: usize, flag: &str) -> Result<usize> {
        if !self.interactive {
            bail!("缺少选项：{prompt}（无人值守模式请指定 {flag}）");
        }
        Ok(Select::with_theme(&ColorfulTheme::default()).with_prompt(prompt).items(items).default(default).interact()?)
    }

    pub fn input(&self, prompt: &str, default: &str, flag: &str) -> Result<String> {
        if !self.interactive {
            bail!("缺少输入：{prompt}（无人值守模式请指定 {flag}）");
        }
        Ok(Input::with_theme(&ColorfulTheme::default())
            .with_prompt(prompt)
            .default(default.to_string())
            .allow_empty(true)
            .interact_text()?)
    }

    pub fn new_password(&self, prompt: &str, flag: &str) -> Result<String> {
        if !self.interactive {
            bail!("缺少密码：{prompt}（无人值守模式请指定 {flag}）");
        }
        Ok(Password::with_theme(&ColorfulTheme::default())
            .with_prompt(prompt)
            .with_confirmation("再次输入", "两次输入不一致")
            .validate_with(|p: &String| if p.chars().count() >= 6 { Ok(()) } else { Err("至少 6 个字符") })
            .interact()?)
    }

    /// Show the planned steps; returns whether to go ahead.
    pub fn plan(&self, title: &str, steps: &[String], dry_run: bool) -> Result<bool> {
        self.section(title);
        for (i, s) in steps.iter().enumerate() {
            println!("  {:>2}. {}", i + 1, s);
        }
        println!();
        if dry_run {
            self.info("--dry-run：仅显示计划，未做任何更改。");
            return Ok(false);
        }
        self.confirm("按以上计划执行？", true)
    }
}

fn short_version() -> &'static str {
    env!("CARGO_PKG_VERSION").split('-').next().unwrap_or("")
}
