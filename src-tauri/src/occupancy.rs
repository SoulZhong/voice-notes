//! 「这篇笔记现在能不能动」:命令入口的准入判定。
//!
//! 一篇笔记可能同时被四件事占着:录制、Aing(AI 整理)、重新转文字(含音频导入的
//! 转写)、补生成成品轨。后两者的后台线程全程持笔记目录锁(`.note.lock`),期间任何
//! 改原始稿/说话人/修订稿的写入都会在存储层被锁拒绝——而锁层只知道"被占用",报出
//! 来的是「录制或转码中」,用户对不上号(2026-09-05 改名"不生效"实为重转写在跑)。
//!
//! 此前各命令各查各的:15 个只查录制,7 个再查 Aing,几乎没有一个查重转写。这里把
//! 「哪种操作被哪种占用挡住」收成一张表([`Intent::blocked_by`]),入口统一问一次,
//! 拒绝时说清楚是被谁挡住。
//!
//! 只管**命令入口的准入**。录制/重转写/Aing/补生成彼此之间占槽后的互查(Dekker
//! 写后读)是另一套协议,在 lib.rs 各自的启动路径里,不经过这里。

/// 一篇笔记此刻的占用快照。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Occupancy {
    pub recording: bool,
    pub refining: bool,
    pub retranscribing: bool,
    pub mixed_regen: bool,
}

/// 挡路的那件事。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Busy {
    Recording,
    Refining,
    Retranscribing,
    MixedRegen,
}

/// 调用方想做的事:决定哪些占用挡路,以及拒绝文案里的动词。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Intent {
    /// 改原始稿段落/说话人(持笔记目录锁写):录制中不行;重转写、补生成持着同一把锁。
    Edit,
    /// 同上,且与 Aing 的产物冲突(修订稿、身份推断、拆分):Aing 收尾整写会吞掉编辑。
    EditOutsideAing,
    /// 保存修订稿:挡路同 EditOutsideAing,只是文案说「保存」。
    SaveRefined,
    /// 录制中也能改(活动笔记走录制 writer,非活动走磁盘):只有持锁的后台任务挡路。
    LiveEdit,
    /// 只写 meta.json(独立的元数据锁,与后台转写不冲突):只有录制挡路。
    EditMeta,
    /// 改标题:录制中不行;重转写期间也拒(转写收尾会重写 meta)。
    Rename,
    /// 删整篇:录制中不行;持锁的后台任务在跑时删目录也会被锁拒。
    Delete,
}

impl Intent {
    /// 按判定顺序列出挡路的占用(同时被几件事占着时,报最先命中的那件)。
    pub(crate) fn blocked_by(self) -> &'static [Busy] {
        use Busy::*;
        match self {
            Intent::Edit => &[Recording, Retranscribing, MixedRegen],
            Intent::EditOutsideAing | Intent::SaveRefined => {
                &[Recording, Refining, Retranscribing, MixedRegen]
            }
            Intent::LiveEdit => &[Retranscribing, MixedRegen],
            Intent::EditMeta => &[Recording],
            Intent::Rename => &[Recording, Retranscribing],
            Intent::Delete => &[Recording, Retranscribing, MixedRegen],
        }
    }

    /// 判定要不要问 Aing(问 Aing 要走一次 actor 往返,不需要就不问)。
    pub(crate) fn needs_refining(self) -> bool {
        self.blocked_by().contains(&Busy::Refining)
    }

    fn verb(self) -> (&'static str, &'static str) {
        match self {
            Intent::Edit | Intent::EditOutsideAing | Intent::LiveEdit | Intent::EditMeta => {
                ("编辑", "edited")
            }
            Intent::SaveRefined => ("保存", "saved"),
            Intent::Rename => ("改名", "renamed"),
            Intent::Delete => ("删除", "deleted"),
        }
    }
}

/// 准入判定(纯函数)。
pub(crate) fn admit(o: Occupancy, intent: Intent) -> Result<(), Busy> {
    for b in intent.blocked_by() {
        let hit = match b {
            Busy::Recording => o.recording,
            Busy::Refining => o.refining,
            Busy::Retranscribing => o.retranscribing,
            Busy::MixedRegen => o.mixed_regen,
        };
        if hit {
            return Err(*b);
        }
    }
    Ok(())
}

/// 拒绝文案:说清楚被谁挡住、什么时候能再来。
pub(crate) fn refusal(busy: Busy, intent: Intent) -> String {
    let (zh, en) = intent.verb();
    let v = if crate::i18n::is_en() { en } else { zh };
    match busy {
        Busy::Recording => crate::tr!("录制中的笔记不能{v}", "A note being recorded cannot be {v}"),
        Busy::Refining => crate::tr!(
            "该笔记正在 Aing 中,整理结束后再{v}",
            "This note is being refined by AI; it can be {v} after that finishes"
        ),
        Busy::Retranscribing => crate::tr!(
            "该笔记正在重新转文字,完成后再{v}",
            "This note is being re-transcribed; it can be {v} after that finishes"
        ),
        Busy::MixedRegen => crate::tr!(
            "该笔记正在生成成品轨,完成后再{v}",
            "This note's mixed track is being regenerated; it can be {v} after that finishes"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Intent; 7] = [
        Intent::Edit,
        Intent::EditOutsideAing,
        Intent::SaveRefined,
        Intent::LiveEdit,
        Intent::EditMeta,
        Intent::Rename,
        Intent::Delete,
    ];

    fn only(b: Busy) -> Occupancy {
        let mut o = Occupancy::default();
        match b {
            Busy::Recording => o.recording = true,
            Busy::Refining => o.refining = true,
            Busy::Retranscribing => o.retranscribing = true,
            Busy::MixedRegen => o.mixed_regen = true,
        }
        o
    }

    /// 整张准入表:行 = 意图,列 = 录制 / Aing / 重转写 / 补生成,true = 挡。
    #[test]
    fn admission_matrix() {
        let table: [(Intent, [bool; 4]); 7] = [
            (Intent::Edit, [true, false, true, true]),
            (Intent::EditOutsideAing, [true, true, true, true]),
            (Intent::SaveRefined, [true, true, true, true]),
            (Intent::LiveEdit, [false, false, true, true]),
            (Intent::EditMeta, [true, false, false, false]),
            (Intent::Rename, [true, false, true, false]),
            (Intent::Delete, [true, false, true, true]),
        ];
        let cols = [
            Busy::Recording,
            Busy::Refining,
            Busy::Retranscribing,
            Busy::MixedRegen,
        ];
        for (intent, row) in table {
            for (b, blocked) in cols.iter().zip(row) {
                assert_eq!(
                    admit(only(*b), intent).is_err(),
                    blocked,
                    "{intent:?} × {b:?}"
                );
            }
        }
    }

    #[test]
    fn idle_note_admits_everything() {
        for intent in ALL {
            assert_eq!(admit(Occupancy::default(), intent), Ok(()), "{intent:?}");
        }
    }

    #[test]
    fn several_busy_reports_the_first_in_order() {
        let o = Occupancy {
            recording: true,
            refining: true,
            retranscribing: true,
            mixed_regen: true,
        };
        assert_eq!(admit(o, Intent::EditOutsideAing), Err(Busy::Recording));
        let o = Occupancy {
            refining: true,
            retranscribing: true,
            ..Default::default()
        };
        assert_eq!(admit(o, Intent::EditOutsideAing), Err(Busy::Refining));
    }

    #[test]
    fn refining_is_only_asked_when_it_can_block() {
        let asks: Vec<Intent> = ALL.into_iter().filter(|i| i.needs_refining()).collect();
        assert_eq!(asks, vec![Intent::EditOutsideAing, Intent::SaveRefined]);
    }

    /// 文案点名是谁挡的:重转写期间不再报成「录制或转码中」。
    #[test]
    fn refusal_names_the_blocker() {
        let m = refusal(Busy::Retranscribing, Intent::Rename);
        assert!(
            m.contains("重新转文字") || m.contains("re-transcribed"),
            "{m}"
        );
        let m = refusal(Busy::MixedRegen, Intent::Edit);
        assert!(m.contains("成品轨") || m.contains("mixed track"), "{m}");
    }
}
