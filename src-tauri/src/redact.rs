//! 上报前的脱敏。与具体上报后端无关,前后端共用同一组规则(TS 侧见 src/lib/redact.ts,
//! 两边用同一组测试向量钉死)。
//!
//! 为什么必须做:spike 实测(2026-08-17)把一条仿真错误发到 PostHog,原样呈现为
//! `refine(note-140751): 写入 /Users/张伟/Library/.../notes/季度复盘会.json 失败`
//! ——家目录里的真实姓名与会议标题一字未漏。异常消息天然会带路径,而本应用的路径
//! 里就有这两样东西,所以这不是"以防万一",是已证实的泄漏面。
//!
//! 规则刻意保守:宁可脱掉有用的信息,也不放过内容。漏网之鱼是"自动脱敏 + 全自动
//! 上报"这条路线的已知代价,已在设计文档中明确接受。

/// 连续多少个中日韩字符就整段丢弃。正文片段最可能以这种形态泄漏,
/// 而界面文案与错误措辞很少这么长。阈值可调,但必须由测试向量钉住。
const CJK_RUN_DROP: usize = 12;

/// 脱敏一段可能进入上报载荷的文本。
pub fn redact(input: &str) -> String {
    let s = redact_home_paths(input);
    let s = redact_api_keys(&s);
    drop_long_cjk_runs(&s)
}

/// 家目录路径收敛为 `<HOME>/…`:`/Users/张伟/x` 与 `/home/zhangwei/x` 都会带出用户名,
/// 而用户名常常就是真实姓名。同时把 notes 目录下的文件名收敛掉——那是会议标题。
fn redact_home_paths(s: &str) -> String {
    let s = redact_windows_home(s);
    let mut out = String::with_capacity(s.len());
    let mut rest = s.as_str();
    while let Some(pos) = rest.find("/Users/").or_else(|| rest.find("/home/")) {
        out.push_str(&rest[..pos]);
        let tail = &rest[pos..];
        // 吃到路径结束。注意 macOS 有 "Application Support" 这种含空格的路径段,
        // 以空白为终点会把后半截漏在外面(实测就漏出了会议标题)。改为:遇到引号、
        // 逗号、分号、换行,或"空格后紧跟非路径样的词"才收尾。
        let end = path_end(tail);
        out.push_str("<HOME_PATH>");
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

/// Windows 家目录:`C:\Users\Alice\...\notes\周会.json`。Windows 是受支持平台,
/// 这类路径同样会带出用户名(常是真实姓名)与 notes 下的会议标题(codex review 发现)。
fn redact_windows_home(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    loop {
        // 找 "<盘符>:\Users\"
        let Some(u) = rest.find(":\\Users\\") else { break };
        // 盘符是它前面那一个字符
        let start = rest[..u].char_indices().next_back().map(|(i, _)| i).unwrap_or(u);
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        let end = tail
            .char_indices()
            .skip(1)
            .find(|(_, c)| matches!(c, '"' | '\'' | ',' | ';' | '\n' | '\r' | ')' | ']') || *c == ' ')
            .map(|(i, _)| i)
            .unwrap_or(tail.len());
        out.push_str("<HOME_PATH>");
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

/// 从 `/Users/...` 起算,找路径的结束位置。含空格的路径段(Application Support)
/// 必须被吃进去,否则后半截会带着会议标题漏出来。
fn path_end(tail: &str) -> usize {
    let bytes: Vec<(usize, char)> = tail.char_indices().collect();
    let mut i = 1;
    let mut end = tail.len();
    while i < bytes.len() {
        let (idx, c) = bytes[i];
        if matches!(c, '"' | '\'' | ',' | ';' | '\n' | '\r' | ')' | ']') {
            end = idx;
            break;
        }
        if c == ' ' {
            // 空格后若不再是路径样的一段(不含 / 且不以大写字母开头),就收尾
            let next: String = bytes[i + 1..].iter().map(|(_, c)| *c).collect();
            let word = next.split_whitespace().next().unwrap_or("");
            let looks_path = word.contains('/') || word.chars().next().is_some_and(|c| c.is_uppercase());
            if !looks_path {
                end = idx;
                break;
            }
        }
        i += 1;
    }
    end
}

/// 形如 sk-/phc_/A-SH- 开头的长串,以及任何 32 位以上的十六进制串。
fn redact_api_keys(s: &str) -> String {
    s.split_whitespace()
        .map(|w| {
            // 用 contains 而非 starts_with:实测 `key=sk-xxx` 这种带前缀的形态会漏网。
            let looks_key = w.contains("sk-")
                || w.contains("phc_")
                || w.contains("phx_")
                || w.contains("A-SH-")
                || (w.len() >= 32 && w.chars().all(|c| c.is_ascii_hexdigit()));
            if looks_key {
                "<REDACTED>".to_string()
            } else {
                w.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// 丢弃过长的中日韩连续串。**整段丢弃而非截断保留前缀**——保留前缀等于保留内容。
fn drop_long_cjk_runs(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut String| {
        if run.chars().count() >= CJK_RUN_DROP {
            out.push_str("<TEXT>");
        } else {
            out.push_str(run);
        }
        run.clear();
    };
    for c in s.chars() {
        if is_cjk(c) {
            run.push(c);
        } else {
            flush(&mut run, &mut out);
            out.push(c);
        }
    }
    flush(&mut run, &mut out);
    out
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x4E00..=0x9FFF      // CJK 统一表意
        | 0x3400..=0x4DBF    // 扩展 A
        | 0x3040..=0x30FF    // 日文假名
        | 0xAC00..=0xD7AF    // 谚文
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// spike 实测泄漏的那条原样样本。它同时含姓名与会议标题,是本模块存在的理由。
    #[test]
    fn 实测泄漏样本被脱干净() {
        let leaked = "refine(note-140751): 写入 /Users/张伟/Library/Application Support/voice-notes/notes/季度复盘会.json 失败";
        let out = redact(leaked);
        assert!(!out.contains("张伟"), "家目录里的姓名必须脱掉: {out}");
        assert!(!out.contains("季度复盘会"), "会议标题必须脱掉: {out}");
        assert!(out.contains("note-140751"), "note-id 不是内容,应保留以便定位: {out}");
        assert!(out.contains("refine"), "模块名应保留: {out}");
    }

    #[test]
    fn windows家目录路径同样收敛() {
        let out = redact("write C:\\Users\\Alice\\AppData\\voice-notes\\notes\\周会.json failed");
        assert!(!out.contains("Alice"), "Windows 用户名必须脱掉: {out}");
        assert!(!out.contains("周会"), "Windows 路径里的会议标题必须脱掉: {out}");
        assert!(out.contains("failed"), "英文措辞应保留: {out}");
    }

    #[test]
    fn 家目录路径两种形态都收敛() {
        assert!(!redact("open /Users/zhangwei/notes/x.json failed").contains("zhangwei"));
        assert!(!redact("open /home/lisi/notes/y.json failed").contains("lisi"));
    }

    #[test]
    fn 长中文串整段丢弃而非截断() {
        let body = "这段话是会议逐字稿的一部分不应该被上报出去";
        let out = redact(&format!("parse failed: {body}"));
        assert!(!out.contains("会议逐字稿"), "不得保留任何前缀: {out}");
        assert!(out.contains("<TEXT>"), "应留占位便于识别: {out}");
        assert!(out.contains("parse failed"), "英文错误措辞应保留: {out}");
    }

    #[test]
    fn 短中文措辞保留便于排查() {
        // 界面/错误里的短中文是有用信息,不该被误伤
        let out = redact("写入失败");
        assert_eq!(out, "写入失败");
    }

    #[test]
    fn 密钥形态被抹掉() {
        assert!(!redact("key=sk-abcdefghijklmnop failed").contains("sk-abcdefghijklmnop"));
        assert!(!redact("phc_qgqdrtaowrPfMPzmD9b7e9JSUPRc3RY3oGAeeKtAAV7E leaked").contains("phc_qgq"));
    }

    #[test]
    fn 无敏感内容时原样通过() {
        let clean = "asr engine returned empty result (code 3)";
        assert_eq!(redact(clean), clean);
    }
}
