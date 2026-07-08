//! Claude Code Agent Skill 的安装/卸载/状态/自愈。模板 include_str! 内嵌,
//! 安装时渲染 {{VERSION}}。受管标记(managed-by)是自愈重写的前提:无标记
//! 视为用户自有文件,自愈绝不覆盖;显式 install 是用户主动操作,总是覆盖。

use std::path::{Path, PathBuf};

const TEMPLATE: &str = include_str!("skill_template.md");
const MANAGED_MARK: &str = "managed-by: voice-notes";

/// 渲染当前版本的 SKILL.md 内容(也是 status 判 stale 的比较基准)。
pub fn rendered() -> String {
    TEMPLATE.replace("{{VERSION}}", env!("CARGO_PKG_VERSION"))
}

fn skill_file(home: &Path) -> PathBuf {
    home.join(".claude/skills/voice-notes/SKILL.md")
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SkillState {
    NotInstalled,
    /// 受管且与当前版本渲染结果一致。
    Current,
    /// 受管但内容过期(旧版本装的/模板改了)——自愈可重写。
    Stale,
    /// 存在同名文件但无受管标记(用户自建/删标记自定义)——一律不动。
    Unmanaged,
}

pub fn status_in(home: &Path) -> SkillState {
    let Ok(text) = std::fs::read_to_string(skill_file(home)) else {
        return SkillState::NotInstalled;
    };
    if !text.contains(MANAGED_MARK) {
        return SkillState::Unmanaged;
    }
    if text == rendered() {
        SkillState::Current
    } else {
        SkillState::Stale
    }
}

pub fn install_in(home: &Path) -> anyhow::Result<()> {
    let file = skill_file(home);
    let dir = file.parent().expect("skill_file 恒有父目录");
    std::fs::create_dir_all(dir)?;
    // tmp+rename 原子写:Agent 可能随时读这个文件,不能让它看到半写状态。
    let tmp = dir.join("SKILL.md.tmp");
    std::fs::write(&tmp, rendered())?;
    std::fs::rename(&tmp, &file)?;
    Ok(())
}

pub fn uninstall_in(home: &Path) -> anyhow::Result<()> {
    let file = skill_file(home);
    if file.exists() {
        std::fs::remove_file(&file)?;
    }
    // 目录只剩壳则顺手删;非空(用户放了别的)会失败,忽略即可。
    if let Some(dir) = file.parent() {
        let _ = std::fs::remove_dir(dir);
    }
    Ok(())
}

/// GUI 启动自愈:仅「受管 + stale」重写。返回是否发生了重写。
pub fn heal_in(home: &Path) -> bool {
    status_in(home) == SkillState::Stale && install_in(home).is_ok()
}

fn real_home() -> anyhow::Result<PathBuf> {
    std::env::var("HOME").map(PathBuf::from).map_err(|_| anyhow::anyhow!("HOME 不可用"))
}

pub fn status() -> anyhow::Result<SkillState> {
    Ok(status_in(&real_home()?))
}

pub fn install() -> anyhow::Result<()> {
    install_in(&real_home()?)
}

pub fn uninstall() -> anyhow::Result<()> {
    uninstall_in(&real_home()?)
}

/// GUI 启动自愈的真实入口。开发机 `cargo run`(exe 路径含 `target` 组件)时
/// 直接 no-op:与 registry::heal 同一裁决——debug 构建的模板未发布,不得覆盖
/// 用户真实安装的 skill。**这意味着开发机上手工验证 heal 行为会看到 no-op**,
/// 需改跑 `heal_in` 单测或临时用已发布 release 二进制验证。
pub fn heal() -> bool {
    if std::env::current_exe()
        .ok()
        .map(|p| p.components().any(|c| c.as_os_str() == "target"))
        .unwrap_or(true)
    {
        return false;
    }
    real_home().map(|h| heal_in(&h)).unwrap_or(false)
}

pub fn cli(args: &[String]) -> i32 {
    match args.first().map(String::as_str).unwrap_or("") {
        "install" => match install() {
            Ok(()) => {
                println!("已安装到 ~/.claude/skills/voice-notes/SKILL.md");
                0
            }
            Err(e) => {
                eprintln!("安装失败: {e}");
                1
            }
        },
        "uninstall" => match uninstall() {
            Ok(()) => {
                println!("已移除");
                0
            }
            Err(e) => {
                eprintln!("移除失败: {e}");
                1
            }
        },
        "status" => {
            match status() {
                Ok(SkillState::NotInstalled) => println!("未安装"),
                Ok(SkillState::Current) => println!("已安装(当前版本)"),
                Ok(SkillState::Stale) => println!("已安装(旧版,应用启动时会自动更新)"),
                Ok(SkillState::Unmanaged) => println!("存在自定义同名 skill(无受管标记,不自动管理)"),
                Err(e) => {
                    eprintln!("查询失败: {e}");
                    return 1;
                }
            }
            0
        }
        _ => {
            eprintln!(
                "用法: voice-notes skill <install|uninstall|status>\n\
                 install     安装 Claude Code 技能(~/.claude/skills/voice-notes/)\n\
                 uninstall   移除\n\
                 status      安装状态"
            );
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendered_substitutes_version_and_keeps_mark() {
        let r = rendered();
        assert!(!r.contains("{{VERSION}}"), "占位必须被替换");
        assert!(r.contains(env!("CARGO_PKG_VERSION")));
        assert!(r.contains(MANAGED_MARK));
        assert!(r.starts_with("---\nname: voice-notes"), "frontmatter 形状");
    }

    #[test]
    fn install_status_uninstall_roundtrip_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(status_in(tmp.path()), SkillState::NotInstalled);
        install_in(tmp.path()).unwrap();
        install_in(tmp.path()).unwrap(); // 幂等
        assert_eq!(status_in(tmp.path()), SkillState::Current);
        uninstall_in(tmp.path()).unwrap();
        uninstall_in(tmp.path()).unwrap(); // 幂等
        assert_eq!(status_in(tmp.path()), SkillState::NotInstalled);
        assert!(!tmp.path().join(".claude/skills/voice-notes").exists(), "空壳目录一并清掉");
    }

    #[test]
    fn stale_is_healed_but_unmanaged_is_never_touched() {
        let tmp = tempfile::tempdir().unwrap();
        // stale:受管标记在,但内容是旧版
        let dir = tmp.path().join(".claude/skills/voice-notes");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), format!("old content\n<!-- {MANAGED_MARK} v0.0.0 -->\n")).unwrap();
        assert_eq!(status_in(tmp.path()), SkillState::Stale);
        assert!(heal_in(tmp.path()), "stale 应被自愈重写");
        assert_eq!(status_in(tmp.path()), SkillState::Current);
        // unmanaged:无标记,自愈绝不动;显式 install 才覆盖
        std::fs::write(dir.join("SKILL.md"), "用户自己的 skill,没有标记").unwrap();
        assert_eq!(status_in(tmp.path()), SkillState::Unmanaged);
        assert!(!heal_in(tmp.path()), "无标记不得自愈");
        assert_eq!(
            std::fs::read_to_string(dir.join("SKILL.md")).unwrap(),
            "用户自己的 skill,没有标记",
            "内容原封不动"
        );
        install_in(tmp.path()).unwrap(); // 显式安装=用户主动,覆盖
        assert_eq!(status_in(tmp.path()), SkillState::Current);
    }
}
