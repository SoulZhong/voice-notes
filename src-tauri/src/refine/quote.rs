//! 引文定位:给一段模型抄回来的原文片段,在段落里找出它的字符区间。
//!
//! 为什么需要(2026-11-24 实测,docs/2026-11-24-llm-call-sites.md):关系抽取与身份
//! 推断的提示词都要求模型报出引文在段内的 `start/end`(Unicode scalar 下标)并且
//! 与 `quote` 逐字符相等。模型数不准字数——5 个月里关系入库 0 条、身份推断采纳
//! 0 条,绝大多数死在「下标对不上」。引文本身通常是对的,错的只是下标。
//!
//! 所以下标只当**提示**用:对得上就用,对不上就按引文内容去找。找到的区间一律以
//! 段落原文为准回填(返回的是真实区间,调用方应把 quote 换成该区间的原文切片),
//! 下游「逐字符相等」的校验因此仍然成立,只是不再依赖模型的算术。

/// 归一匹配时忽略的字符:空白与**句内**标点。模型抄引文时最常见的偏差就是增删空格
/// (精修给中英文之间加了空格)和全半角标点互换。
///
/// 句末标点(。!?;及其半角)**不忽略**:忽略它们会把两句话缝成一句——原文
/// 「不是张三。负责灯塔计划的是李四」会被引文「张三负责灯塔计划」命中,
/// 「这不是我,是张伟提的」会被「我是张伟」命中。逗号同理不忽略:它同样分隔语义。
fn is_skippable(c: char) -> bool {
    if matches!(c, '.' | '!' | '?' | ';' | ',' | '。' | '!' | '?' | ';' | ',' | '…') {
        return false;
    }
    c.is_whitespace()
        || c.is_ascii_punctuation()
        || matches!(
            c,
            '、' | ':' | '—' | '·' | '“' | '”' | '‘' | '’' | '「' | '」' | '『' | '』'
                | '(' | ')' | '《' | '》' | '【' | '】'
        )
}

/// 归一匹配要求的最少有效字符数。太短的引文(一两个字)去掉标点后到处都能撞上,
/// 定位结果不可信。
const MIN_NORMALIZED_CHARS: usize = 4;

/// `hay` 中 `needle` 的全部出现位置(字符下标,可重叠)。
fn find_all(hay: &[char], needle: &[char]) -> Vec<usize> {
    if needle.is_empty() || needle.len() > hay.len() {
        return Vec::new();
    }
    (0..=hay.len() - needle.len())
        .filter(|&i| hay[i..i + needle.len()] == *needle)
        .collect()
}

/// 多处命中时挑离提示位置最近的一处;没有提示取第一处。
fn pick_nearest(starts: &[usize], hint_start: Option<usize>) -> Option<usize> {
    match hint_start {
        Some(h) => starts.iter().copied().min_by_key(|&s| s.abs_diff(h)),
        None => starts.first().copied(),
    }
}

/// 逐字定位:`hint` 区间正好等于 `quote` 就用它,否则取离 `hint` 起点最近的逐字出现。
fn locate_exact(hay: &[char], needle: &[char], hint: Option<(usize, usize)>) -> Option<(usize, usize)> {
    if needle.is_empty() {
        return None;
    }
    if let Some((start, end)) = hint {
        if start < end && end <= hay.len() && hay[start..end] == *needle {
            return Some((start, end));
        }
    }
    let start = pick_nearest(&find_all(hay, needle), hint.map(|(start, _)| start))?;
    Some((start, start + needle.len()))
}

/// 归一定位:两边都滤掉可忽略字符并小写化后匹配,映射回原文区间(从首个匹配字符
/// 到末个匹配字符)。
fn locate_normalized(hay: &[char], needle: &[char], hint: Option<(usize, usize)>) -> Option<(usize, usize)> {
    // hay 侧保留每个保留字符在原文中的下标。
    let (hay_norm, hay_pos): (Vec<char>, Vec<usize>) = hay
        .iter()
        .enumerate()
        .filter(|(_, c)| !is_skippable(**c))
        .map(|(i, c)| (lower(*c), i))
        .unzip();
    let needle_norm: Vec<char> = needle.iter().filter(|c| !is_skippable(**c)).map(|c| lower(*c)).collect();
    if needle_norm.len() < MIN_NORMALIZED_CHARS {
        return None;
    }
    let starts = find_all(&hay_norm, &needle_norm);
    // 提示是原文下标,先换算成归一后的下标再比远近。
    let hint_norm = hint.map(|(start, _)| hay_pos.partition_point(|&p| p < start));
    let start_norm = pick_nearest(&starts, hint_norm)?;
    let start = hay_pos[start_norm];
    let end = hay_pos[start_norm + needle_norm.len() - 1] + 1;
    Some((start, end))
}

/// 在 `text` 里定位 `quote`,返回字符半开区间 `[start, end)`。
///
/// 依次尝试:
/// 1. `hint` 给的区间逐字符等于 `quote` —— 模型报对了,原样采用;
/// 2. `quote` 在段内的逐字出现 —— 多处时取离 `hint` 起点最近的;
/// 3. 忽略空白与句内标点后的出现 —— 映射回原文区间。
///
/// 都找不到返回 `None`:引文在这段里不存在,调用方应把它当作不可信证据。
pub(crate) fn locate(text: &str, quote: &str, hint: Option<(usize, usize)>) -> Option<(usize, usize)> {
    let hay: Vec<char> = text.chars().collect();
    let needle: Vec<char> = quote.chars().collect();
    locate_exact(&hay, &needle, hint).or_else(|| locate_normalized(&hay, &needle, hint))
}

/// 单字符小写化(只取首个映射字符,够应付拉丁字母大小写差异)。
fn lower(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// 取 `text` 的字符区间切片。区间由 [`locate`] 给出时必然合法。
pub(crate) fn slice(text: &str, start: usize, end: usize) -> String {
    text.chars().skip(start).take(end.saturating_sub(start)).collect()
}

/// 跨段定位。逐字出现优先于归一匹配,而且这个优先级**跨段成立**:
/// 1. 模型指的那一段里逐字出现 → 用它;
/// 2. 引文在全文**恰好一处**逐字出现 → 认那一段(模型把段落下标报错了);多处不猜;
/// 3. 模型指的那一段里归一匹配(忽略空白与句内标点)→ 用它。
///
/// 第 2 步排在第 3 步之前:下标报偏一段、而两段措辞相近时,别处的逐字原文比本段的
/// 近似匹配可信——证据挂错段会连带挂错说话人。换段不做归一匹配。
///
/// 返回 `(段落下标, start, end)`。
pub(crate) fn locate_in_paragraphs<'a>(
    paragraphs: impl Iterator<Item = &'a str> + Clone,
    paragraph_index: usize,
    quote: &str,
    hint: Option<(usize, usize)>,
) -> Option<(usize, usize, usize)> {
    let needle: Vec<char> = quote.chars().collect();
    let named: Option<Vec<char>> = paragraphs.clone().nth(paragraph_index).map(|text| text.chars().collect());
    if let Some(hay) = &named {
        if let Some((start, end)) = locate_exact(hay, &needle, hint) {
            return Some((paragraph_index, start, end));
        }
    }
    if significant_len(&needle) >= MIN_NORMALIZED_CHARS {
        let mut found: Option<(usize, usize, usize)> = None;
        let mut ambiguous = false;
        for (index, text) in paragraphs.enumerate() {
            let hay: Vec<char> = text.chars().collect();
            let hits = find_all(&hay, &needle);
            if hits.len() > 1 || (hits.len() == 1 && found.is_some()) {
                ambiguous = true;
                break;
            }
            if let Some(&start) = hits.first() {
                found = Some((index, start, start + needle.len()));
            }
        }
        if ambiguous {
            return None;
        }
        if found.is_some() {
            return found;
        }
    }
    let hay = named?;
    locate_normalized(&hay, &needle, hint).map(|(start, end)| (paragraph_index, start, end))
}

/// 去掉空白与标点后的字符数。
fn significant_len(chars: &[char]) -> usize {
    chars.iter().filter(|c| !is_skippable(**c)).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn correct_hint_is_taken_as_is() {
        assert_eq!(locate("今天张三负责灯塔计划", "张三负责", Some((2, 6))), Some((2, 6)));
    }

    #[test]
    fn wrong_offsets_are_recovered_from_the_quote() {
        // 模型把下标报成 (0, 4),引文其实在 (2, 6)。
        assert_eq!(locate("今天张三负责灯塔计划", "张三负责", Some((0, 4))), Some((2, 6)));
        // 完全没给下标。
        assert_eq!(locate("今天张三负责灯塔计划", "张三负责", None), Some((2, 6)));
        // 下标越界。
        assert_eq!(locate("今天张三负责灯塔计划", "张三负责", Some((90, 99))), Some((2, 6)));
    }

    #[test]
    fn repeated_quote_picks_the_occurrence_nearest_the_hint() {
        let text = "我们上线。然后我们上线。";
        assert_eq!(locate(text, "我们上线", Some((6, 10))), Some((7, 11)));
        assert_eq!(locate(text, "我们上线", Some((1, 5))), Some((0, 4)));
        assert_eq!(locate(text, "我们上线", None), Some((0, 4)));
    }

    #[test]
    fn whitespace_and_punctuation_drift_still_locates_and_returns_the_real_span() {
        // 原文中英文之间有空格、书名用了书名号;模型抄的时候丢了空格和书名号。
        let text = "下周用 Claude Code 上线《灯塔计划》,张三负责。";
        let span = locate(text, "用Claude Code上线灯塔计划", None).unwrap();
        assert_eq!(slice(text, span.0, span.1), "用 Claude Code 上线《灯塔计划");
        // 大小写差异。
        let span = locate(text, "用 claude code 上线", None).unwrap();
        assert_eq!(slice(text, span.0, span.1), "用 Claude Code 上线");
    }

    #[test]
    fn absent_or_too_short_quotes_are_not_located() {
        assert_eq!(locate("今天张三负责灯塔计划", "李四负责灯塔", None), None);
        assert_eq!(locate("今天张三负责灯塔计划", "", None), None);
        // 归一后只剩三个字:不靠归一匹配去猜(逐字出现仍然算数)。
        assert_eq!(locate("好 的 行", "好的行", None), None);
        assert_eq!(locate("好的,行。", "好的", None), Some((0, 2)));
    }

    /// 审查发现:忽略句末标点会把两句话缝成一句,造出原文没有的意思。
    #[test]
    fn normalized_match_never_stitches_across_sentence_or_clause_boundaries() {
        assert_eq!(locate("不是张三。负责灯塔计划的是李四", "张三负责灯塔计划", None), None);
        assert_eq!(locate("这不是我,是张伟提的", "我是张伟", None), None);
        assert_eq!(locate("这不是我,是张伟提的", "我是张伟", None), None);
        assert_eq!(locate("张三?负责灯塔计划吗", "张三负责灯塔计划", None), None);
    }

    /// 审查发现:下标报偏一段时,别处的逐字原文应压过本段的近似匹配。
    #[test]
    fn exact_match_elsewhere_beats_normalized_match_in_the_named_paragraph() {
        let paragraphs = ["张三 负责 灯塔计划", "张三负责灯塔计划"];
        assert_eq!(
            locate_in_paragraphs(paragraphs.iter().copied(), 0, "张三负责灯塔计划", None),
            Some((1, 0, 8))
        );
        // 别处没有逐字原文时,本段的归一匹配仍然可用。
        assert_eq!(
            locate_in_paragraphs(["张三 负责 灯塔计划", "散会"].iter().copied(), 0, "张三负责灯塔计划", None),
            Some((0, 0, 10))
        );
    }

    #[test]
    fn wrong_paragraph_index_falls_back_to_the_unique_paragraph() {
        let paragraphs = ["先聊预算。", "张三负责灯塔计划。", "散会。"];
        assert_eq!(
            locate_in_paragraphs(paragraphs.iter().copied(), 0, "张三负责灯塔计划", None),
            Some((1, 0, 8))
        );
        // 指对了段就不换段。
        assert_eq!(
            locate_in_paragraphs(paragraphs.iter().copied(), 1, "张三负责", Some((0, 4))),
            Some((1, 0, 4))
        );
        // 下标越界同样走全文唯一匹配。
        assert_eq!(
            locate_in_paragraphs(paragraphs.iter().copied(), 99, "张三负责灯塔计划", None),
            Some((1, 0, 8))
        );
    }

    #[test]
    fn ambiguous_or_short_quotes_do_not_jump_paragraphs() {
        let paragraphs = ["张三负责灯塔计划。", "对,张三负责灯塔计划。", "散会。"];
        // 两段都有:不猜。
        assert_eq!(locate_in_paragraphs(paragraphs.iter().copied(), 2, "张三负责灯塔计划", None), None);
        // 太短:不换段。
        assert_eq!(locate_in_paragraphs(["你好。", "散会。"].iter().copied(), 0, "散会", None), None);
    }
}
