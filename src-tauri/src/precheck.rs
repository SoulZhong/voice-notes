//! 开录前风险判定。
//!
//! 由来(2026-08-17):一场 22 分 54 秒的在线会议丢了 14.2% 的时长(蓝牙 HFP 麦克风
//! 间歇断流)外加约 27% 被系统「语音突显」削成绝对零。后者当时**横幅正在显示**,
//! 用户照录了——被动横幅这个形式已被实证无效,而这两个条件开录前就能知道。
//!
//! 本模块只做判定,不做拦截:拦在哪、怎么拦是装配层与前端的事。判定与展示分开,
//! 是为了让「有没有风险」这一个真值能被多个入口(按钮、将来的托盘/快捷键提示)
//! 共用,而不是各自拼一遍。
//!
//! 刻意不做通用风险框架:只收本次有实测证据的两条。输入音量过低(用户一键可修且
//! 不损坏内容)、蓝牙输出回声风险(影响回声消除效果而非内容完整性)量级不同,
//! 仍留在既有横幅里。

use crate::audio::mic_mode::MicMode;

/// 一条开录前风险。`kind` 是稳定机读标识,前端按它选文案与改法链接,不要按 detail 判。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RecordRisk {
    pub kind: String,
    /// 补充事实(如设备名);没有额外可说的就是空串。前端可展示但不得据此判定。
    pub detail: String,
}

/// 系统「语音突显」开着:把它判定为非人声的部分削成绝对零,判错时连人声一起削。
pub const KIND_VOICE_ISOLATION: &str = "voice_isolation";
/// 默认输入设备是蓝牙:HFP 上行与会议软件争带宽,会间歇断流。
pub const KIND_BLUETOOTH_MIC: &str = "bluetooth_mic";

/// 纯判定:把已读到的系统状态组装成风险列表。抽成纯函数是为了能被表驱动测试直接打
/// ——系统查询本身在真机上返回什么不可控,但"什么状态该报什么风险"必须是确定的。
///
/// 顺序即展示顺序:语音突显排前面,它的损失比例更大(实测 ~27% vs 14.2%)且改起来
/// 更快(控制中心两下),先说更划算的那条。
pub fn record_risks(mode: MicMode, input_is_bluetooth: bool) -> Vec<RecordRisk> {
    let mut out = Vec::new();
    if mode.damages_audio() {
        out.push(RecordRisk { kind: KIND_VOICE_ISOLATION.into(), detail: String::new() });
    }
    if input_is_bluetooth {
        out.push(RecordRisk { kind: KIND_BLUETOOTH_MIC.into(), detail: String::new() });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_system_reports_no_risk() {
        assert!(record_risks(MicMode::Standard, false).is_empty());
        // 宽谱是"少处理",比标准还保真,不该报。
        assert!(record_risks(MicMode::WideSpectrum, false).is_empty());
        // 读不到麦克风模式时不猜:宁可不提示,也不能凭空拦住用户开录。
        assert!(record_risks(MicMode::Unknown, false).is_empty());
    }

    #[test]
    fn voice_isolation_alone_is_reported() {
        let r = record_risks(MicMode::VoiceIsolation, false);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].kind, KIND_VOICE_ISOLATION);
    }

    #[test]
    fn bluetooth_mic_alone_is_reported() {
        let r = record_risks(MicMode::Standard, true);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].kind, KIND_BLUETOOTH_MIC);
    }

    /// 2026-08-17 那场就是两条同时成立。两条都要报——只报一条会让用户改完一个
    /// 以为没事了,另一半损失照旧。
    #[test]
    fn both_risks_are_reported_together_in_display_order() {
        let r = record_risks(MicMode::VoiceIsolation, true);
        assert_eq!(
            r.iter().map(|x| x.kind.as_str()).collect::<Vec<_>>(),
            vec![KIND_VOICE_ISOLATION, KIND_BLUETOOTH_MIC],
            "语音突显排前面:损失更大且改起来更快"
        );
    }

    /// 前端按 kind 选文案,改动即破坏契约。
    #[test]
    fn kind_identifiers_are_stable() {
        assert_eq!(KIND_VOICE_ISOLATION, "voice_isolation");
        assert_eq!(KIND_BLUETOOTH_MIC, "bluetooth_mic");
    }
}
