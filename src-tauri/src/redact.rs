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
    let s = redact_file_urls(input);
    let s = redact_windows_paths(&s);
    let s = redact_unix_paths(&s);
    let s = redact_api_keys(&s);
    drop_long_cjk_runs(&s)
}

/// `file:///Users/Alice/notes/周会.json`。**必须单独一条**:通用扫描要求 `/` 前面不是
/// `:` 或 `/`(否则 `tauri://localhost/…` 会被吃掉),而 `file://` 恰好三个斜杠连排,
/// 三个候选起点全被那条规则挡掉,整条路径原样放行(codex review 二轮 P1#3)。
/// 只认 `file:` —— http(s) 是接口地址而不是本机路径,留着有用。
fn redact_file_urls(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.to_ascii_lowercase().find("file://") {
        out.push_str(&rest[..pos]);
        let tail = &rest[pos..];
        let end = path_end(tail);
        out.push_str("<PATH>");
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

/// 绝对路径收敛。家目录一档(`<HOME_PATH>`)、其余一档(`<PATH>`)。
///
/// **为什么家目录之外的也要收**:数据目录与模型目录都可以被用户指到任何地方——
/// `/Volumes/客户名/季度复盘/`、网络盘、外置盘。迁移失败与模型加载失败的错误消息里
/// 带的正是那些路径,而它们既不在家目录下、中文串又常常短于整段丢弃的阈值,
/// 于是会原样出站(codex review P1#3)。
fn redact_unix_paths(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = find_path_start(rest) {
        out.push_str(&rest[..pos]);
        let tail = &rest[pos..];
        // 吃到路径结束。注意 macOS 有 "Application Support" 这种含空格的路径段,
        // 以空白为终点会把后半截漏在外面(实测就漏出了会议标题)。改为:遇到引号、
        // 逗号、分号、换行,或"空格后紧跟非路径样的词"才收尾。
        let end = path_end(tail);
        let seg = &tail[..end];
        out.push_str(if seg.starts_with("/Users/") || seg.starts_with("/home/") {
            "<HOME_PATH>"
        } else {
            "<PATH>"
        });
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

/// 路径起点:一个 `/`,前面是行首或分隔符,且这条路径至少两段。
///
/// **前一个字符不能是 `:` 或 `/`**——否则 `tauri://localhost/notes/note-1` 这类
/// URL 会被整条吃掉。URL 里没有用户内容(路由动态段一律是 id,见设计文档),
/// 却是定位前端异常现场的依据,不该误伤。同理要求至少两段:`a/b` 这种相对写法
/// 与孤零零一个 `/` 都不是路径。
fn find_path_start(s: &str) -> Option<usize> {
    for (i, c) in s.char_indices() {
        if c != '/' {
            continue;
        }
        if i > 0 {
            let Some(prev) = s[..i].chars().next_back() else { continue };
            // 冒号后面**只有 `://` 才是 scheme**。一律排除冒号的话,`C:/Users/Alice/…`
            // (正斜杠写法的盘符路径)与 `copy:/Volumes/客户/…` 这类"标签:路径"会整条
            // 漏网,而里面的短用户名与目录名都够不到整段丢弃的阈值(codex review 三轮)。
            let scheme_sep = prev == ':' && s[i..].starts_with("//");
            let boundary = matches!(
                prev,
                ' ' | '\t' | '"' | '\'' | '(' | '[' | ',' | ';' | '=' | '\n' | '\r'
            ) || (prev == ':' && !scheme_sep);
            if !boundary {
                continue;
            }
        }
        let end = path_end(&s[i..]);
        if s[i..i + end].matches('/').count() >= 2 {
            return Some(i);
        }
    }
    None
}

/// Windows 绝对路径:`C:\Users\Alice\...\notes\周会.json`,以及自定义目录可能落在的
/// `D:\客户\...`。Windows 是受支持平台,这类路径同样会带出用户名(常是真实姓名)与
/// notes 下的会议标题(codex review 发现)。
///
/// 终点判据与 unix 分支**共用 `path_end`**。此前这里是另一套(任何空格即终点),
/// 于是 `C:\Users\Alice\Meeting Notes\Q3 roadmap.json` 会在第一个空格处停下,
/// 把后半截连同会议标题漏在外面(codex review P1#4)。
fn redact_windows_paths(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    loop {
        // 盘符路径 "<盘符>:\"(盘符是 `:` 前面那一个 ASCII 字母),或 UNC "\\server\share\"
        // ——网络盘正是"自定义数据目录能落在哪儿"里最典型的一种(codex review 二轮 P1#3)。
        let drive = rest.char_indices().find_map(|(i, c)| {
            if !c.is_ascii_alphabetic() {
                return None;
            }
            rest[i + c.len_utf8()..].starts_with(":\\").then_some(i)
        });
        // **要接着往后找**,不能只看第一对:Debug/JSON 形态的 `\\\\server\\share\\…`
        // 第一对后面仍是反斜杠,`find().filter()` 会当场判否并放弃搜索,整条路径原样
        // 留下——而 TS 侧的正则会继续往后走,两端还因此不等价(codex review 四轮 P1)。
        let unc = rest
            .match_indices("\\\\")
            .map(|(i, _)| i)
            .find(|i| rest[i + 2..].starts_with(|c: char| c != '\\'));
        let Some(start) = [drive, unc].into_iter().flatten().min() else {
            break;
        };
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        let end = path_end(tail);
        let seg = &tail[..end];
        // `C:\Users\` 才算家目录,`D:\客户\` 只是普通绝对路径——两档都不出站,
        // 但占位符分开,看板上能区分"用户名泄漏面"与"自定义目录面"。
        // UNC 不算家目录(`\\server\share\…` 的第 3 段是共享名,不是用户名)。
        let is_home = !seg.starts_with("\\\\")
            && seg.len() > 2
            && seg[2..].to_ascii_lowercase().starts_with("\\users\\");
        out.push_str(if is_home { "<HOME_PATH>" } else { "<PATH>" });
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
    // "还在目录段里"这条规则能吃几个空格。**必须有上限**:不封顶的话,
    // `copy /Volumes/客户/季度 复盘 failed because disk full` 会被整条吞成
    // `copy <PATH>`——路径确实脱干净了,可排查线索也一起没了(codex review 三轮 P2)。
    // 1 个足够覆盖"两词目录名";三词以上的目录名,中间那些词含 `/`,由 looks_path
    // 接住,不占这个额度。
    let mut dir_space_budget = 1u8;
    while i < bytes.len() {
        let (idx, c) = bytes[i];
        if matches!(c, '"' | '\'' | ',' | ';' | '\n' | '\r' | ')' | ']') {
            end = idx;
            break;
        }
        if c == ' ' {
            // 空格后这个词还属于路径吗?会议标题里常有空格(如 "Q3 roadmap.json"),
            // 只看"是否大写开头"会在 `Q3 roadmap.json` 处停下、把 roadmap.json 漏出去
            // (codex review 第二轮发现)。判据改为:含 / 、含扩展名点、或大写开头
            // ——三者任一都说明还在路径里面。
            let next: String = bytes[i + 1..].iter().map(|(_, c)| *c).collect();
            let word = next.split_whitespace().next().unwrap_or("");
            // 还没走到带扩展名的文件名 ⇒ 仍在目录段里,空格大概率是目录名的一部分。
            // `/Volumes/客户/季度 复盘` 里的"复盘"既不大写开头也无扩展名,只靠下面三条
            // 判据会留在外面,而两个字的中文串远短于整段丢弃阈值,会原样出站
            // (codex review 二轮 P1#3)。反过来 `…/y.json 写入失败` 已经有扩展名,
            // 就在空格处收尾,把措辞留给排查用。
            let in_dir_segment = !tail[..idx]
                .rsplit(['/', '\\'])
                .next()
                .is_some_and(|seg| seg.contains('.'));
            let looks_path = word.contains('/')
                || word.contains('\\')
                || word.rsplit('.').next().is_some_and(|ext| {
                    !ext.is_empty() && ext.len() <= 5 && ext.chars().all(|c| c.is_ascii_alphanumeric())
                        && word.contains('.')
                })
                || word.chars().next().is_some_and(|c| c.is_uppercase());
            if !looks_path {
                // 再往前看一眼:硬终点之前还有像路径的词吗?有就说明这是个多词文件名
                // (`weekly product roadmap review.json`),整段都还在路径里。
                // 只看下一个词的话,四词以上的标题会从第二个空格处漏出后半截
                // ——那正是"额度封顶 1 个"引入的回归(codex review 四轮 P1)。
                if has_path_ahead(&next) {
                    // 走这条不扣额度:多词文件名有明确证据,不是靠猜。
                } else if in_dir_segment && dir_space_budget > 0 {
                    // 没有证据、又还在目录段里:让一个空格,覆盖"两词目录名"。
                    // 额度用完就收尾,否则 `…/季度 复盘 failed because disk full`
                    // 会被整条吞掉,排查线索一起没了。
                    dir_space_budget -= 1;
                } else {
                    end = idx;
                    break;
                }
            }
        }
        i += 1;
    }
    end
}

/// 硬终点之前还有像路径的词吗?**只认硬证据**(含分隔符、或带扩展名),不认
/// "大写开头"——那条判据对一个词够用,放到整段前瞻上太松,会把整句英文措辞吞掉。
/// 前瞻到硬终点为止:逗号/引号/换行之后已经是另一件事了。
fn has_path_ahead(rest: &str) -> bool {
    let head: &str = rest
        .split(|c| matches!(c, '"' | '\'' | ',' | ';' | '\n' | '\r' | ')' | ']'))
        .next()
        .unwrap_or("");
    head.split_whitespace().any(|w| {
        w.contains('/')
            || w.contains('\\')
            || (w.contains('.')
                && w.rsplit('.').next().is_some_and(|ext| {
                    !ext.is_empty()
                        && ext.len() <= 5
                        && ext.chars().all(|c| c.is_ascii_alphanumeric())
                }))
    })
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

    /// 会议标题里常有空格。codex review 第二轮发现:原判据会在 "Q3 roadmap.json"
    /// 的空格处停下,把 roadmap.json 漏出去。
    #[test]
    fn 含空格的笔记文件名整条脱掉() {
        let out = redact("write /Users/Alice/Library/voice-notes/notes/Q3 roadmap.json failed");
        assert!(!out.contains("roadmap"), "带空格的会议标题必须整条脱掉: {out}");
        assert!(!out.contains("Alice"), "用户名必须脱掉: {out}");
        assert!(out.contains("failed"), "尾部英文措辞应保留: {out}");
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

    /// 数据目录/模型目录可被指到任何地方。这类路径既不在家目录下,里面的中文串
    /// 又常常短于"整段丢弃"的阈值——不收就原样出站(codex review P1#3)。
    #[test]
    fn 家目录之外的绝对路径同样收敛() {
        let out = redact("迁移失败: 复制 /Volumes/客户名/季度复盘/note.json 失败");
        assert!(!out.contains("客户名"), "外置盘上的目录名必须脱掉: {out}");
        assert!(!out.contains("季度复盘"), "路径里的会议名必须脱掉: {out}");
        assert!(out.contains("<PATH>"), "应留占位便于识别: {out}");
    }

    #[test]
    fn windows非家目录的绝对路径同样收敛() {
        let out = redact("migrate failed: D:\\客户\\周会.json unreachable");
        assert!(!out.contains("客户"), "盘符路径里的目录名必须脱掉: {out}");
        assert!(!out.contains("周会"), "盘符路径里的会议名必须脱掉: {out}");
    }

    /// Windows 分支此前是另一套终点判据(任何空格即终点),含空格的路径会漏后半截。
    #[test]
    fn windows含空格的路径不漏后半截() {
        let out = redact("write C:\\Users\\Alice\\Meeting Notes\\Q3 roadmap.json failed");
        assert!(!out.contains("Meeting"), "含空格的目录名必须整条脱掉: {out}");
        assert!(!out.contains("roadmap"), "会议标题必须整条脱掉: {out}");
        assert!(out.contains("failed"), "尾部英文措辞应保留: {out}");
    }

    /// URL 不是路径:里面没有用户内容(路由动态段一律是 id),却是定位现场的依据。
    /// file:// 的三个斜杠连排,把通用扫描的三个候选起点全挡掉了。
    #[test]
    fn file协议路径同样收敛() {
        let out = redact("load failed: file:///Users/Alice/notes/周会.json");
        assert!(!out.contains("Alice"), "file:// 里的用户名必须脱掉: {out}");
        assert!(!out.contains("周会"), "file:// 里的会议标题必须脱掉: {out}");
    }

    /// 网络盘正是"自定义数据目录能落在哪儿"里最典型的一种。
    #[test]
    fn unc网络路径同样收敛() {
        let out = redact("migrate failed: \\\\server\\share\\客户\\周会.json");
        assert!(!out.contains("客户"), "UNC 里的目录名必须脱掉: {out}");
        assert!(!out.contains("周会"), "UNC 里的会议名必须脱掉: {out}");
    }

    /// 目录段里的空格:两个字的中文远短于整段丢弃阈值,只靠 CJK 规则接不住。
    #[test]
    fn 无扩展名目录里的空格不截断路径() {
        let out = redact("copy /Volumes/客户/季度 复盘 failed");
        assert!(!out.contains("复盘"), "目录名后半截必须一起脱掉: {out}");
    }

    /// 反过来:已经走到带扩展名的文件名,就该在空格处收尾,把措辞留给排查。
    #[test]
    fn 文件名之后的措辞保留() {
        let out = redact("write /Users/Alice/notes/x.json 写入失败");
        assert!(out.contains("写入失败"), "扩展名之后的措辞应保留: {out}");
        assert!(!out.contains("Alice"), "{out}");
    }

    /// 冒号后只有 `://` 才是 scheme。一律排除冒号会把这两类整条放过。
    #[test]
    fn 冒号后的路径不被当成url放过() {
        let out = redact("copy C:/Users/Alice/notes/周会.json failed");
        assert!(!out.contains("Alice"), "正斜杠盘符路径必须脱掉: {out}");
        assert!(!out.contains("周会"), "{out}");
        let out2 = redact("copy:/Volumes/客户/季度复盘/x.json");
        assert!(!out2.contains("客户"), "标签:路径 必须脱掉: {out2}");
    }

    /// 目录段规则必须封顶,否则路径脱干净了、排查线索也一起没了。
    #[test]
    fn 目录段空格规则不吞掉整段消息() {
        let out = redact("copy /Volumes/客户/季度 复盘 failed because disk full");
        assert!(!out.contains("复盘"), "目录名必须脱掉: {out}");
        assert!(out.contains("failed"), "错误措辞必须留下: {out}");
        assert!(out.contains("disk full"), "{out}");
    }

    /// 连续空格:Rust 的 split_whitespace 跳过全部空白,TS 的 split(/\s/) 会拿到空串。
    /// 这条向量两端都跑,钉住那处不等价。
    #[test]
    fn 连续空格处不提前收尾() {
        let out = redact("write /Users/Alice/notes/Q3.v1  roadmap.json failed");
        assert!(!out.contains("roadmap"), "连续空格后的文件名必须脱掉: {out}");
        assert!(!out.contains("Alice"), "{out}");
    }

    /// 多词文件名:只看下一个词的话,四词以上的标题会从第二个空格处漏出后半截。
    #[test]
    fn 多词文件名整条脱掉() {
        let out = redact("write /Users/Alice/notes/weekly product roadmap review.json failed");
        assert!(!out.contains("roadmap"), "多词标题必须整条脱掉: {out}");
        assert!(!out.contains("review"), "{out}");
        assert!(!out.contains("Alice"), "{out}");
        assert!(out.contains("failed"), "文件名之后的措辞仍应保留: {out}");
    }

    /// Debug/JSON 形态的 UNC:第一对反斜杠后面仍是反斜杠,不能就此放弃搜索。
    #[test]
    fn 转义形态的unc同样收敛() {
        let out = redact(r"migrate failed: \\server\share\客户\周会.json");
        assert!(!out.contains("客户"), "转义 UNC 里的目录名必须脱掉: {out}");
        assert!(!out.contains("周会"), "{out}");
    }

    #[test]
    fn url不被当成路径误伤() {
        let clean = "load failed at tauri://localhost/notes/note-140751";
        assert_eq!(redact(clean), clean);
    }

    #[test]
    fn 无敏感内容时原样通过() {
        let clean = "asr engine returned empty result (code 3)";
        assert_eq!(redact(clean), clean);
    }
}
