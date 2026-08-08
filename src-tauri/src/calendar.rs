//! P3 日历(macOS):EventKit 访问层。
//!
//! `EKEventStore` 是 `!Send + !Sync`——store 只活在专用 `calendar-worker` 串行
//! 线程里,对外经 channel 收发纯 Rust 数据;这同时天然充当并发门(停止挂钩、
//! backfill、改选查询全部排队)。每个请求包在 autoreleasepool 里,防止长跑
//! worker 积累 Objective-C 临时对象。
//! 全部按 objc2-event-kit 0.3.2 生成签名调用(不猜 selector);可空返回
//! (eventIdentifier/name/absoluteString)显式处理,无 identifier 的事件跳过。

// include! 的共享文件自带测试模块,后续条目触发 items_after_test_module 风格提示,
// 结构是刻意的(双平台同形),按文件豁免。
#![allow(clippy::items_after_test_module)]
include!("calendar_common.rs");

use std::sync::mpsc::{channel, Sender};
use std::sync::OnceLock;
use std::time::Duration;

use objc2::rc::autoreleasepool;
use objc2::runtime::Bool;
use objc2_event_kit::{
    EKAuthorizationStatus, EKEntityType, EKEventStore, EKParticipantStatus, EKParticipantType,
};
use objc2_foundation::{NSDate, NSError, NSOperatingSystemVersion, NSProcessInfo};

enum Req {
    Request(Sender<AuthOutcome>),
    Events {
        start_ms: i64,
        end_ms: i64,
        reply: Sender<anyhow::Result<Vec<EventInfo>>>,
    },
}

fn worker() -> &'static Sender<Req> {
    static TX: OnceLock<Sender<Req>> = OnceLock::new();
    TX.get_or_init(|| {
        let (tx, rx) = channel::<Req>();
        std::thread::Builder::new()
            .name("calendar-worker".into())
            .spawn(move || {
                // store 在本线程创建、只在本线程使用(!Send 约束的唯一合法姿势)。
                let store = unsafe { EKEventStore::new() };
                for req in rx {
                    autoreleasepool(|_| match req {
                        Req::Request(reply) => {
                            let _ = reply.send(do_request(&store));
                        }
                        Req::Events { start_ms, end_ms, reply } => {
                            let _ = reply.send(do_events(&store, start_ms, end_ms));
                        }
                    });
                }
            })
            .expect("spawn calendar-worker");
        tx
    })
}

/// 授权态查询:类方法线程安全,无需过 worker。
pub fn permission_status() -> Permission {
    let st = unsafe { EKEventStore::authorizationStatusForEntityType(EKEntityType::Event) };
    // macOS 13 的 Authorized 与 14+ 的 FullAccess 同值(=3),一并映射 Full;
    // Restricted(家长控制/MDM)用户自己也开不了,视同 Denied。
    if st == EKAuthorizationStatus::FullAccess {
        Permission::Full
    } else if st == EKAuthorizationStatus::WriteOnly {
        Permission::WriteOnly
    } else if st == EKAuthorizationStatus::NotDetermined {
        Permission::NotDetermined
    } else {
        Permission::Denied
    }
}

/// 发起系统授权(必须由用户动作触发;调用方应在 spawn_blocking 内)。
pub fn request_permission() -> AuthOutcome {
    let (tx, rx) = channel();
    if worker().send(Req::Request(tx)).is_err() {
        return AuthOutcome::Error;
    }
    // 65s:worker 内部对 completion 等 60s,这里再放宽一点,超时归 Timeout。
    rx.recv_timeout(Duration::from_secs(65)).unwrap_or(AuthOutcome::Timeout)
}

/// 时间窗内事件(需已授权;未授权时 EventKit 返回空列表,由调用方先查状态)。
pub fn events_between(start_ms: i64, end_ms: i64) -> anyhow::Result<Vec<EventInfo>> {
    let (tx, rx) = channel();
    worker()
        .send(Req::Events { start_ms, end_ms, reply: tx })
        .map_err(|_| anyhow::anyhow!("calendar worker 已退出"))?;
    rx.recv_timeout(Duration::from_secs(30))
        .map_err(|_| anyhow::anyhow!("calendar worker 查询超时"))?
}

fn do_request(store: &EKEventStore) -> AuthOutcome {
    let (tx, rx) = channel::<(bool, bool)>(); // (granted, had_error)
    let block = block2::RcBlock::new(move |granted: Bool, err: *mut NSError| {
        let had_error = !err.is_null();
        if had_error {
            let desc = unsafe { (*err).localizedDescription() };
            eprintln!("calendar: 授权请求返回错误: {desc}");
        }
        let _ = tx.send((granted.as_bool(), had_error));
    });
    let modern = NSProcessInfo::processInfo().isOperatingSystemAtLeastVersion(
        NSOperatingSystemVersion { majorVersion: 14, minorVersion: 0, patchVersion: 0 },
    );
    unsafe {
        let ptr = &*block as *const block2::DynBlock<dyn Fn(Bool, *mut NSError)>
            as *mut block2::DynBlock<dyn Fn(Bool, *mut NSError)>;
        if modern {
            store.requestFullAccessToEventsWithCompletion(ptr);
        } else {
            // macOS 13:新 selector 不存在,走当时的官方 API(14+ 上已弃用)。
            #[allow(deprecated)]
            store.requestAccessToEntityType_completion(EKEntityType::Event, ptr);
        }
    }
    match rx.recv_timeout(Duration::from_secs(60)) {
        Ok((true, _)) => AuthOutcome::Granted,
        Ok((false, true)) => AuthOutcome::Error,
        // granted=false 且无错误:用户拒绝,或系统只给了只写权限。
        Ok((false, false)) => match permission_status() {
            Permission::WriteOnly => AuthOutcome::Insufficient,
            _ => AuthOutcome::Denied,
        },
        Err(_) => AuthOutcome::Timeout,
    }
}

fn do_events(store: &EKEventStore, start_ms: i64, end_ms: i64) -> anyhow::Result<Vec<EventInfo>> {
    anyhow::ensure!(start_ms < end_ms, "非法时间窗");
    unsafe {
        let start = NSDate::dateWithTimeIntervalSince1970(start_ms as f64 / 1000.0);
        let end = NSDate::dateWithTimeIntervalSince1970(end_ms as f64 / 1000.0);
        let pred = store.predicateForEventsWithStartDate_endDate_calendars(&start, &end, None);
        let events = store.eventsMatchingPredicate(&pred);
        let mut out = Vec::new();
        for ev in events.iter() {
            // 无 identifier 的事件无法进改选协议,跳过。
            let Some(id) = ev.eventIdentifier() else { continue };
            let title = ev.title().to_string();
            let start_ms = (ev.startDate().timeIntervalSince1970() * 1000.0) as i64;
            let end_ms = (ev.endDate().timeIntervalSince1970() * 1000.0) as i64;
            let mut attendees = Vec::new();
            if let Some(parts) = ev.attendees() {
                for p in parts.iter() {
                    // 会议室/设备资源与已拒绝者不是"参会人先验"。
                    let ty = p.participantType();
                    if ty == EKParticipantType::Room || ty == EKParticipantType::Resource {
                        continue;
                    }
                    if p.participantStatus() == EKParticipantStatus::Declined {
                        continue;
                    }
                    let name = p.name().map(|n| n.to_string()).unwrap_or_default();
                    let email = p
                        .URL()
                        .absoluteString()
                        .map(|u| normalize_email(&u.to_string()))
                        .unwrap_or_default();
                    if name.is_empty() && email.is_empty() {
                        continue;
                    }
                    attendees.push(crate::store::CalendarAttendee {
                        name,
                        email,
                        is_me: p.isCurrentUser(),
                    });
                }
            }
            out.push(EventInfo { event_id: id.to_string(), title, start_ms, end_ms, all_day: ev.isAllDay(), attendees });
        }
        Ok(out)
    }
}
