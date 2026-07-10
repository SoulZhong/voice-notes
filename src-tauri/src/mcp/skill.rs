//! Claude Code Agent Skill 的安装/卸载/状态/自愈。模板 include_str! 内嵌,
//! 安装时渲染 {{VERSION}}。受管标记(managed-by)是自愈重写的前提:无标记
//! 视为用户自有文件,自愈绝不覆盖;显式 install 是用户主动操作,总是覆盖。

use std::path::{Path, PathBuf};

const TEMPLATE: &str = include_str!("skill_template.md");
const MANAGED_MARK: &str = "managed-by: voice-notes";

/// 安装时二进制的绝对路径,填进模板的 CLI 降级命令。用 current_exe 而非硬编码
/// `/Applications/...`:装在别处、开发态构建、App 移动位置后,模板里的路径才指向
/// 真实二进制;App 移动后 rendered() 变→已装 skill 变 stale→启动自愈重写(与注册
/// 路径自愈同理)。current_exe 取不到时回落标准安装路径。
fn binary_path() -> String {
    std::env::current_exe()
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "/Applications/voice-notes.app/Contents/MacOS/voice-notes".into())
}

/// 渲染当前版本的 SKILL.md 内容(也是 status 判 stale 的比较基准)。
pub fn rendered() -> String {
    TEMPLATE
        .replace("{{VERSION}}", env!("CARGO_PKG_VERSION"))
        .replace("{{BINARY}}", &binary_path())
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

/// 读取安装文件;未安装时给出渲染稿供预览/首次编辑(state 仍如实为 NotInstalled)。
pub fn read_in(home: &Path) -> (String, SkillState) {
    let state = status_in(home);
    if state == SkillState::NotInstalled {
        return (rendered(), SkillState::NotInstalled);
    }
    let content = std::fs::read_to_string(skill_file(home)).unwrap_or_else(|_| rendered());
    (content, state)
}

/// 剥离受管标记所在整行;若剥离后该行原先的前后邻居都是空行,顺带吞掉一行,
/// 避免文中留一个双空行的空洞。无标记时原样返回(不受影响)。
fn strip_managed_mark(content: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let Some(idx) = lines.iter().position(|l| l.contains(MANAGED_MARK)) else {
        return content.to_string();
    };
    let mut out: Vec<&str> = Vec::with_capacity(lines.len().saturating_sub(1));
    out.extend_from_slice(&lines[..idx]);
    out.extend_from_slice(&lines[idx + 1..]);
    if idx > 0 && idx < out.len() && out[idx - 1].trim().is_empty() && out[idx].trim().is_empty() {
        out.remove(idx);
    }
    let mut result = out.join("\n");
    if content.ends_with('\n') && !result.is_empty() {
        result.push('\n');
    }
    result
}

/// 保存 = 编辑即接管:剥离受管标记 → 目录按需创建 → tmp+rename 原子写。
/// 保存后 `status_in` 自然判 Unmanaged(不含标记),升级自愈不再触碰此文件。
pub fn save_in(home: &Path, content: &str) -> anyhow::Result<()> {
    if content.trim().is_empty() {
        anyhow::bail!("内容为空");
    }
    let stripped = strip_managed_mark(content);
    let file = skill_file(home);
    let dir = file.parent().expect("skill_file 恒有父目录");
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join("SKILL.md.tmp");
    std::fs::write(&tmp, &stripped)?;
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

pub fn read() -> anyhow::Result<(String, SkillState)> {
    Ok(read_in(&real_home()?))
}

pub fn save(content: &str) -> anyhow::Result<()> {
    save_in(&real_home()?, content)
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

    #[test]
    fn read_in_not_installed_returns_rendered_default() {
        let tmp = tempfile::tempdir().unwrap();
        let (content, state) = read_in(tmp.path());
        assert_eq!(state, SkillState::NotInstalled);
        assert_eq!(content, rendered());
    }

    #[test]
    fn save_in_strips_managed_mark_and_becomes_unmanaged() {
        let tmp = tempfile::tempdir().unwrap();
        install_in(tmp.path()).unwrap();
        assert_eq!(status_in(tmp.path()), SkillState::Current);

        let edited = rendered().replace("会议纪要", "会议纪要(我加的)");
        assert!(edited.contains(MANAGED_MARK), "编辑稿仍带标记,交给 save_in 剥离");
        save_in(tmp.path(), &edited).unwrap();

        assert_eq!(status_in(tmp.path()), SkillState::Unmanaged, "保存后应即刻变自管");
        let saved = std::fs::read_to_string(skill_file(tmp.path())).unwrap();
        assert!(!saved.contains(MANAGED_MARK), "受管标记行必须被剥离");
        assert!(saved.contains("会议纪要(我加的)"), "用户内容保留");
        assert!(!saved.contains("\n\n\n"), "标记行前后的空行需归一,不留双空行空洞");
    }

    #[test]
    fn save_in_without_mark_saved_verbatim() {
        let tmp = tempfile::tempdir().unwrap();
        let content = "用户完全自定义的 skill 正文,没有受管标记。";
        save_in(tmp.path(), content).unwrap();
        assert_eq!(std::fs::read_to_string(skill_file(tmp.path())).unwrap(), content);
        assert_eq!(status_in(tmp.path()), SkillState::Unmanaged);
    }

    #[test]
    fn save_in_rejects_blank_content() {
        let tmp = tempfile::tempdir().unwrap();
        let err = save_in(tmp.path(), "   \n\t\n").unwrap_err();
        assert!(err.to_string().contains("内容为空"));
        assert_eq!(status_in(tmp.path()), SkillState::NotInstalled, "拒绝时不应落盘");
    }

    #[test]
    fn save_in_creates_missing_dir_atomically_and_roundtrips_via_read_in() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!tmp.path().join(".claude").exists());
        let content = "首次以自管身份落盘的内容";
        save_in(tmp.path(), content).unwrap();
        let (read_back, state) = read_in(tmp.path());
        assert_eq!(state, SkillState::Unmanaged);
        assert_eq!(read_back, content);
        // tmp 文件不应残留(原子写完成)
        assert!(!skill_file(tmp.path()).parent().unwrap().join("SKILL.md.tmp").exists());
    }
}
