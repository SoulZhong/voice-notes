//! E3 spike(issue #99 二期):macOS 14.2+ **私有聚合设备 + CATap** 的双轨同时钟采集探针。
//!
//! 要回答的是路线 A 的成色:把 mic 子设备与系统输出的 process tap 塞进同一个私有聚合设备、
//! 开 drift 补偿、单 IOProc 出两路——两轨是否真的共用一个采样时钟(残余漂移趋近 0),
//! 代价(延迟/CPU/授权/热插拔)能不能接受。判据在 docs/2026-08-12-clock-drift-sensor-design.md
//! 第五节,已预注册,不许事后改。
//!
//! spike 代码允许粗糙(这是探针不是产品):回调里 push Vec 会分配、错误处理靠 unwrap/日志。
//! 产品化要等裁决结果出来再说,**绝不能**把这条未验证的采集路径先接进主干(见 issue #99 留言)。
//!
//! 用法(cd src-tauri):
//!   列输入设备:     cargo run --example catap_spike -- list
//!   能力/授权探针:   cargo run --example catap_spike -- probe
//!   双轨采集:        cargo run --example catap_spike -- capture --secs 120 --out /tmp/catap
//!   残余漂移(E1 口径,注意 system 在前):
//!                    cargo run --bin xcorr_align -- /tmp/catap/system.wav /tmp/catap/mic.wav
//!
//! probe 只建/拆设备不落音频,是最便宜的一步:CATap 走 TCC「音频录制」授权,
//! 从终端跑时继承的是终端的授权身份,失败码本身就是判据 4(授权面)的数据。

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("catap_spike 仅 macOS(CATap 需要 14.2+)");
}

#[cfg(target_os = "macos")]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().cloned().unwrap_or_else(|| "probe".into());
    let r = match cmd.as_str() {
        "list" => mac::list_inputs(),
        "probe" => mac::probe(&args),
        "capture" => mac::capture(&args),
        other => Err(anyhow::anyhow!("未知子命令 {other}(用 list|probe|capture)")),
    };
    if let Err(e) = r {
        eprintln!("失败: {e:#}");
        std::process::exit(1);
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use anyhow::{anyhow, bail, Context, Result};
    use coreaudio::sys::*;
    use objc2::runtime::{AnyClass, AnyObject, Bool};
    use objc2::{class, msg_send};
    use std::ffi::{c_void, CString};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Instant;

    // ── 14.2+ 才有的两个 C 函数:绑定(coreaudio-sys 0.2.18)里没有,自己 dlsym 取。
    // 不直接 extern 声明是刻意的:dlsym 取不到 == 这台机器不支持路线 A,这本身是探针
    // 要输出的第一个结论;直接链接则会变成 dyld 启动即失败,拿不到可读的结论。
    type CreateProcessTapFn = unsafe extern "C" fn(*mut AnyObject, *mut AudioObjectID) -> OSStatus;
    type DestroyProcessTapFn = unsafe extern "C" fn(AudioObjectID) -> OSStatus;

    fn dlsym_fn(name: &str) -> Option<*mut c_void> {
        let c = CString::new(name).ok()?;
        // RTLD_DEFAULT = 全局搜索已加载镜像(CoreAudio 已被 coreaudio-sys 链进来)。
        let p = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c.as_ptr()) };
        (!p.is_null()).then_some(p)
    }

    fn create_tap_fn() -> Option<CreateProcessTapFn> {
        dlsym_fn("AudioHardwareCreateProcessTap").map(|p| unsafe { std::mem::transmute(p) })
    }
    fn destroy_tap_fn() -> Option<DestroyProcessTapFn> {
        dlsym_fn("AudioHardwareDestroyProcessTap").map(|p| unsafe { std::mem::transmute(p) })
    }

    // ── CoreFoundation 小工具(用 coreaudio-sys 里带出来的 CF 绑定,不再引新依赖) ──

    fn cfstr(s: &str) -> CFStringRef {
        let c = CString::new(s).expect("字符串含 NUL");
        unsafe { CFStringCreateWithCString(std::ptr::null(), c.as_ptr(), kCFStringEncodingUTF8) }
    }

    fn cfnum_i32(v: i32) -> CFNumberRef {
        unsafe {
            CFNumberCreate(
                std::ptr::null(),
                kCFNumberSInt32Type as CFNumberType,
                &v as *const i32 as *const c_void,
            )
        }
    }

    fn cfstring_to_string(s: CFStringRef) -> Option<String> {
        if s.is_null() {
            return None;
        }
        unsafe {
            let len = CFStringGetLength(s);
            let max = CFStringGetMaximumSizeForEncoding(len, kCFStringEncodingUTF8) + 1;
            let mut buf = vec![0i8; max as usize];
            let ok = CFStringGetCString(s, buf.as_mut_ptr(), max, kCFStringEncodingUTF8);
            (ok != 0).then(|| {
                let bytes: Vec<u8> = buf.iter().take_while(|b| **b != 0).map(|b| *b as u8).collect();
                String::from_utf8_lossy(&bytes).into_owned()
            })
        }
    }

    /// key: &[u8; N] 形式的 C 字面量(bindgen 把 #define "taps" 生成成这个样子)。
    fn cfkey(k: &[u8]) -> CFStringRef {
        let s = std::str::from_utf8(&k[..k.len() - 1]).expect("key 非 utf8");
        cfstr(s)
    }

    unsafe fn cfdict(pairs: &[(CFStringRef, *const c_void)]) -> CFDictionaryRef {
        let mut keys: Vec<*const c_void> = pairs.iter().map(|(k, _)| *k as *const c_void).collect();
        let mut vals: Vec<*const c_void> = pairs.iter().map(|(_, v)| *v).collect();
        CFDictionaryCreate(
            std::ptr::null(),
            keys.as_mut_ptr(),
            vals.as_mut_ptr(),
            pairs.len() as CFIndex,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        )
    }

    unsafe fn cfarray(items: &[*const c_void]) -> CFArrayRef {
        let mut items = items.to_vec();
        CFArrayCreate(
            std::ptr::null(),
            items.as_mut_ptr(),
            items.len() as CFIndex,
            &kCFTypeArrayCallBacks,
        )
    }

    // ── AudioObject 属性读取 ──

    fn addr(selector: u32, scope: u32) -> AudioObjectPropertyAddress {
        AudioObjectPropertyAddress {
            mSelector: selector,
            mScope: scope,
            mElement: kAudioObjectPropertyElementMain,
        }
    }

    fn get_prop<T: Copy>(obj: AudioObjectID, a: &AudioObjectPropertyAddress) -> Result<T> {
        let mut size = std::mem::size_of::<T>() as u32;
        let mut out = std::mem::MaybeUninit::<T>::uninit();
        let st = unsafe {
            AudioObjectGetPropertyData(obj, a, 0, std::ptr::null(), &mut size, out.as_mut_ptr() as *mut c_void)
        };
        if st != 0 {
            bail!("读属性 {:#x} 失败: OSStatus {st} ({})", a.mSelector, os_status_hint(st));
        }
        Ok(unsafe { out.assume_init() })
    }

    fn get_prop_string(obj: AudioObjectID, a: &AudioObjectPropertyAddress) -> Result<String> {
        let s: CFStringRef = get_prop(obj, a)?;
        let out = cfstring_to_string(s).ok_or_else(|| anyhow!("CFString 转换失败"));
        if !s.is_null() {
            unsafe { CFRelease(s as *const c_void) };
        }
        out
    }

    /// 变长属性(如 StreamConfiguration 的 AudioBufferList)。返回原始字节。
    fn get_prop_bytes(obj: AudioObjectID, a: &AudioObjectPropertyAddress) -> Result<Vec<u8>> {
        let mut size: u32 = 0;
        let st = unsafe { AudioObjectGetPropertyDataSize(obj, a, 0, std::ptr::null(), &mut size) };
        if st != 0 {
            bail!("取属性大小 {:#x} 失败: OSStatus {st} ({})", a.mSelector, os_status_hint(st));
        }
        let mut buf = vec![0u8; size as usize];
        let st = unsafe {
            AudioObjectGetPropertyData(obj, a, 0, std::ptr::null(), &mut size, buf.as_mut_ptr() as *mut c_void)
        };
        if st != 0 {
            bail!("读属性 {:#x} 失败: OSStatus {st} ({})", a.mSelector, os_status_hint(st));
        }
        Ok(buf)
    }

    /// 常见 OSStatus 的人话(四字符码按大端解出来更好认)。
    fn os_status_hint(st: OSStatus) -> String {
        let b = (st as u32).to_be_bytes();
        let fourcc: String = b.iter().map(|c| if c.is_ascii_graphic() { *c as char } else { '.' }).collect();
        let known = match st {
            0 => "ok",
            560947818 => "!obj 对象不存在",
            1852797029 => "nope 非法操作(常见于无权限/系统不支持)",
            561211770 => "!dat 数据非法",
            -50 => "参数错误",
            _ => "",
        };
        format!("'{fourcc}' {known}")
    }

    // ── 输入设备枚举 ──

    struct DevInfo {
        id: AudioObjectID,
        uid: String,
        name: String,
        in_channels: u32,
        nominal_hz: f64,
    }

    fn channels_in(dev: AudioObjectID) -> u32 {
        let a = addr(kAudioDevicePropertyStreamConfiguration, kAudioObjectPropertyScopeInput);
        let Ok(bytes) = get_prop_bytes(dev, &a) else { return 0 };
        buffer_list_channels(&bytes).iter().sum()
    }

    /// 把 AudioBufferList 的原始字节解成"每个 buffer 的声道数"。
    /// 布局:UInt32 mNumberBuffers + N × AudioBuffer{UInt32 ch, UInt32 size, ptr}。
    fn buffer_list_channels(bytes: &[u8]) -> Vec<u32> {
        if bytes.len() < 4 {
            return vec![];
        }
        let n = u32::from_ne_bytes(bytes[0..4].try_into().unwrap()) as usize;
        let mut out = Vec::with_capacity(n);
        // AudioBufferList 里 mBuffers 起始处按指针对齐:32 位 count + 4 字节 padding。
        let stride = std::mem::size_of::<AudioBuffer>();
        let base = std::mem::size_of::<AudioBufferList>() - stride;
        for i in 0..n {
            let off = base + i * stride;
            if off + 4 > bytes.len() {
                break;
            }
            out.push(u32::from_ne_bytes(bytes[off..off + 4].try_into().unwrap()));
        }
        out
    }

    fn all_devices() -> Result<Vec<AudioObjectID>> {
        let a = addr(kAudioHardwarePropertyDevices, kAudioObjectPropertyScopeGlobal);
        let bytes = get_prop_bytes(kAudioObjectSystemObject, &a)?;
        Ok(bytes
            .chunks_exact(4)
            .map(|c| u32::from_ne_bytes(c.try_into().unwrap()))
            .collect())
    }

    fn dev_info(id: AudioObjectID) -> DevInfo {
        let uid = get_prop_string(id, &addr(kAudioDevicePropertyDeviceUID, kAudioObjectPropertyScopeGlobal))
            .unwrap_or_default();
        let name = get_prop_string(id, &addr(kAudioObjectPropertyName, kAudioObjectPropertyScopeGlobal))
            .unwrap_or_default();
        let nominal_hz: f64 =
            get_prop(id, &addr(kAudioDevicePropertyNominalSampleRate, kAudioObjectPropertyScopeGlobal))
                .unwrap_or(0.0);
        DevInfo { id, uid, name, in_channels: channels_in(id), nominal_hz }
    }

    fn default_input() -> Result<DevInfo> {
        let id: AudioObjectID = get_prop(
            kAudioObjectSystemObject,
            &addr(kAudioHardwarePropertyDefaultInputDevice, kAudioObjectPropertyScopeGlobal),
        )?;
        Ok(dev_info(id))
    }

    pub fn list_inputs() -> Result<()> {
        let def = default_input().ok();
        println!("输入设备(★ = 当前默认):");
        for id in all_devices()? {
            let d = dev_info(id);
            if d.in_channels == 0 {
                continue;
            }
            let star = if def.as_ref().map(|x| x.id) == Some(id) { "★" } else { " " };
            println!("{star} {:<38} {:>6.0}Hz {}ch  uid={}", d.name, d.nominal_hz, d.in_channels, d.uid);
        }
        Ok(())
    }

    // ── CATapDescription(ObjC,14.2+) ──

    /// 全局 tap:抓系统所有进程的输出混音。exclude 传空数组 = 谁都不排除。
    /// 选择器按可用性依次尝试并把实际用到的那个打出来——spike 跑在没法预先验证 API 形状的
    /// 环境里,与其猜错静默失败,不如把探测过程写进输出。
    fn make_tap_description() -> Result<*mut AnyObject> {
        let Some(cls) = AnyClass::get(c"CATapDescription") else {
            bail!("运行时找不到 CATapDescription 类:本机 CoreAudio 不支持进程 tap(需 macOS 14.2+)");
        };
        let empty: *mut AnyObject = unsafe { msg_send![class!(NSArray), array] };
        let alloc: *mut AnyObject = unsafe { msg_send![cls, alloc] };
        if alloc.is_null() {
            bail!("CATapDescription alloc 返回 nil");
        }
        // 立体声全局 tap(排除空列表)。老一点的系统上只有 initExcludingProcesses:,故两条都试。
        // 全局立体声 tap。注意没有 initExcludingProcesses: 这个短形式(实测方法表里只有
        // initExcludingProcesses:andDeviceUID:withStream:),所以选择器不在就直接报错,
        // 不瞎发——发不认识的 selector = ObjC 异常 = 进程 abort。
        let sel_stereo = objc2::sel!(initStereoGlobalTapButExcludeProcesses:);
        if !cls.responds_to(sel_stereo) {
            bail!("CATapDescription 没有 initStereoGlobalTapButExcludeProcesses:(系统版本形状不符,先跑 probe 看方法表)");
        }
        step("init initStereoGlobalTapButExcludeProcesses:");
        let desc: *mut AnyObject = unsafe { msg_send![alloc, initStereoGlobalTapButExcludeProcesses: empty] };
        if desc.is_null() {
            bail!("CATapDescription 初始化返回 nil");
        }
        // 下面三个属性一律先 respondsToSelector: 再发——CATapDescription 的属性集随系统版本
        // 变动,直接发不认识的 selector 会抛 ObjC 异常,而 Rust 接不住外来异常,整个进程 abort
        // (实测:第一版就这么炸的)。探针要的是"哪些能用"的结论,不是崩溃。
        // 私有 tap:只有建它的进程看得见,不在系统声音设置里露出、不打扰其它 app。
        // 真名是 setPrivate:(getter isPrivate)——不是文档直觉里的 setPrivateTap:,
        // 这是 probe 的方法表 dump 给出的事实。
        if responds(desc, objc2::sel!(setPrivate:)) {
            step("setPrivate:YES");
            let _: () = unsafe { msg_send![desc, setPrivate: Bool::YES] };
        } else {
            println!("  ⚠ 无 setPrivate:,tap 会对其它 app 可见");
        }
        // 不静音被 tap 的进程:要的是"用户照常听得见"的场景(kCATapUnmuted = 0)。
        // NSInteger = i64,不能传 i32(ABI 不同)。
        if responds(desc, objc2::sel!(setMuteBehavior:)) {
            step("setMuteBehavior:0(unmuted)");
            let _: () = unsafe { msg_send![desc, setMuteBehavior: 0i64] };
        }
        if responds(desc, objc2::sel!(setName:)) {
            step("setName:");
            let name = nsstring("voice-notes E3 spike tap");
            let _: () = unsafe { msg_send![desc, setName: name] };
        }
        Ok(desc)
    }

    /// 把 CATapDescription 的实例方法名全打出来。这个类没有公开头文件可查版本差异,
    /// 属性名一猜错就是「unrecognized selector → ObjC 异常 → 进程 abort」(第一版实测),
    /// 所以探针直接问运行时要真名单。
    fn dump_tap_selectors() {
        let Some(cls) = AnyClass::get(c"CATapDescription") else { return };
        let mut names: Vec<String> = cls
            .instance_methods()
            .iter()
            .map(|m| m.name().name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        println!("CATapDescription 实例方法({} 个):", names.len());
        for chunk in names.chunks(4) {
            println!("  {}", chunk.join("  "));
        }
    }

    /// 逐步打点:外来异常会直接 abort 进程,只有"发之前就打印"才知道死在哪一句。
    fn step(what: &str) {
        use std::io::Write;
        println!("  → {what}");
        let _ = std::io::stdout().flush();
    }

    fn responds(obj: *mut AnyObject, sel: objc2::runtime::Sel) -> bool {
        let b: Bool = unsafe { msg_send![obj, respondsToSelector: sel] };
        b.as_bool()
    }

    fn nsstring(s: &str) -> *mut AnyObject {
        let c = CString::new(s).expect("含 NUL");
        unsafe { msg_send![class!(NSString), stringWithUTF8String: c.as_ptr()] }
    }

    /// tap 描述里的 UUID 字符串——聚合设备的 taps 列表按它引用这个 tap。
    fn tap_uuid_string(desc: *mut AnyObject) -> Result<String> {
        if !responds(desc, objc2::sel!(UUID)) {
            bail!("CATapDescription 没有 UUID 属性,拿不到聚合设备引用它所需的 uid");
        }
        step("UUID / UUIDString");
        let uuid: *mut AnyObject = unsafe { msg_send![desc, UUID] };
        if uuid.is_null() {
            bail!("tap 描述没有 UUID");
        }
        let s: *mut AnyObject = unsafe { msg_send![uuid, UUIDString] };
        let utf8: *const i8 = unsafe { msg_send![s, UTF8String] };
        if utf8.is_null() {
            bail!("UUIDString 取不到 UTF8");
        }
        Ok(unsafe { std::ffi::CStr::from_ptr(utf8) }.to_string_lossy().into_owned())
    }

    struct Tap {
        id: AudioObjectID,
        uuid: String,
    }

    impl Drop for Tap {
        fn drop(&mut self) {
            if let Some(f) = destroy_tap_fn() {
                let st = unsafe { f(self.id) };
                if st != 0 {
                    eprintln!("销毁 tap 失败: {st} ({})", os_status_hint(st));
                }
            }
        }
    }

    fn create_tap() -> Result<Tap> {
        let create = create_tap_fn()
            .ok_or_else(|| anyhow!("dlsym 取不到 AudioHardwareCreateProcessTap:本机不支持路线 A(需 14.2+)"))?;
        let desc = make_tap_description()?;
        let uuid = tap_uuid_string(desc)?;
        let mut id: AudioObjectID = 0;
        let st = unsafe { create(desc, &mut id) };
        if st != 0 {
            bail!(
                "创建 process tap 失败: OSStatus {st} ({})——若是 'nope',多半是 TCC 音频录制授权没给\
                 (从终端跑时继承的是终端的授权身份),这条本身就是判据 4 的数据",
                os_status_hint(st)
            );
        }
        Ok(Tap { id, uuid })
    }

    /// tap 自己的流格式(kAudioTapPropertyFormat):采样率/声道数,用来对账聚合设备的通道布局。
    fn tap_format(tap: AudioObjectID) -> Result<AudioStreamBasicDescription> {
        get_prop(tap, &addr(kAudioTapPropertyFormat, kAudioObjectPropertyScopeGlobal))
    }

    // ── 私有聚合设备 ──

    struct Aggregate {
        id: AudioObjectID,
    }

    impl Drop for Aggregate {
        fn drop(&mut self) {
            let st = unsafe { AudioHardwareDestroyAggregateDevice(self.id) };
            if st != 0 {
                eprintln!("销毁聚合设备失败: {st} ({})", os_status_hint(st));
            }
        }
    }

    /// mic 作 master(时钟源),tap 挂在 taps 列表并开 drift 补偿——路线 A 的全部赌注就在
    /// 这个 drift 键上:系统替我们把 tap 侧重采样到 master 时钟。
    fn create_aggregate(mic_uid: &str, tap_uuid: &str) -> Result<Aggregate> {
        unsafe {
            let sub = cfdict(&[(cfkey(kAudioSubDeviceUIDKey), cfstr(mic_uid) as *const c_void)]);
            let subs = cfarray(&[sub as *const c_void]);
            let tap = cfdict(&[
                (cfkey(kAudioSubTapUIDKey), cfstr(tap_uuid) as *const c_void),
                (cfkey(kAudioSubTapDriftCompensationKey), cfnum_i32(1) as *const c_void),
            ]);
            let taps = cfarray(&[tap as *const c_void]);
            let desc = cfdict(&[
                (cfkey(kAudioAggregateDeviceNameKey), cfstr("voice-notes E3 spike") as *const c_void),
                (
                    cfkey(kAudioAggregateDeviceUIDKey),
                    cfstr("com.teemo.voice-notes.e3-spike") as *const c_void,
                ),
                (cfkey(kAudioAggregateDeviceIsPrivateKey), cfnum_i32(1) as *const c_void),
                (cfkey(kAudioAggregateDeviceMainSubDeviceKey), cfstr(mic_uid) as *const c_void),
                (cfkey(kAudioAggregateDeviceSubDeviceListKey), subs as *const c_void),
                (cfkey(kAudioAggregateDeviceTapListKey), taps as *const c_void),
            ]);
            let mut id: AudioObjectID = 0;
            let st = AudioHardwareCreateAggregateDevice(desc, &mut id);
            CFRelease(desc as *const c_void);
            if st != 0 {
                bail!("创建私有聚合设备失败: OSStatus {st} ({})", os_status_hint(st));
            }
            Ok(Aggregate { id })
        }
    }

    // ── probe:只建/拆,不采音 ──

    pub fn probe(_args: &[String]) -> Result<()> {
        println!("== E3 探针:CATap + 私有聚合设备 ==");
        println!("系统: {}", std::process::Command::new("sw_vers")
            .arg("-productVersion")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .unwrap_or_default()
            .trim());
        println!(
            "AudioHardwareCreateProcessTap: {}",
            if create_tap_fn().is_some() { "有(系统支持)" } else { "无(14.2 以下,路线 A 直接出局)" }
        );
        println!("CATapDescription 类: {}", if AnyClass::get(c"CATapDescription").is_some() { "有" } else { "无" });
        dump_tap_selectors();

        let mic = default_input().context("取默认输入设备失败")?;
        println!("默认输入: {} ({}ch @{:.0}Hz) uid={}", mic.name, mic.in_channels, mic.nominal_hz, mic.uid);

        let t0 = Instant::now();
        let tap = create_tap()?;
        println!("创建 tap 成功: id={} uuid={} 耗时 {:?}", tap.id, tap.uuid, t0.elapsed());
        match tap_format(tap.id) {
            Ok(f) => println!(
                "tap 格式: {:.0}Hz {}ch {}bit(flags {:#x})",
                f.mSampleRate, f.mChannelsPerFrame, f.mBitsPerChannel, f.mFormatFlags
            ),
            Err(e) => println!("tap 格式读不到: {e:#}"),
        }

        let t1 = Instant::now();
        let agg = create_aggregate(&mic.uid, &tap.uuid)?;
        println!("创建私有聚合设备成功: id={} 耗时 {:?}", agg.id, t1.elapsed());

        let a = addr(kAudioDevicePropertyStreamConfiguration, kAudioObjectPropertyScopeInput);
        let chans = buffer_list_channels(&get_prop_bytes(agg.id, &a)?);
        println!("聚合设备输入通道布局: {chans:?}(按 subdevices→taps 顺序,capture 据此切轨)");
        let hz: f64 = get_prop(agg.id, &addr(kAudioDevicePropertyNominalSampleRate, kAudioObjectPropertyScopeGlobal))?;
        println!("聚合设备标称率: {hz:.0}Hz(应等于 master=mic 的标称率)");
        if let Ok(lat) = get_prop::<u32>(agg.id, &addr(kAudioDevicePropertyLatency, kAudioObjectPropertyScopeInput)) {
            println!("输入延迟: {lat} 帧 ≈ {:.1}ms", lat as f64 * 1000.0 / hz.max(1.0));
        }
        if let Ok(safety) =
            get_prop::<u32>(agg.id, &addr(kAudioDevicePropertySafetyOffset, kAudioObjectPropertyScopeInput))
        {
            println!("安全偏移: {safety} 帧");
        }
        println!("拆设备…(Drop 里做,失败会打日志)");
        Ok(())
    }

    // ── capture:单 IOProc 双轨落盘 ──

    /// 采集期共享状态。IOProc 跑在实时线程,这里的 push 会分配——spike 容忍,
    /// 产品化必须换成预分配环形缓冲。
    struct Shared {
        mic_ch: usize,
        tap_ch: usize,
        src_hz: f64,
        mic: Vec<f32>,
        sys: Vec<f32>,
        callbacks: u64,
        frames: u64,
        /// 每次回调的墙钟耗时(µs),用于 CPU/抖动统计。
        cb_us: Vec<u32>,
        /// 输入时间戳不连续的次数与最大跳变(帧)——判据 2「错位曲线无跳变」的直接证据:
        /// 系统若在内部做硬校正/丢帧,这里会先看到 mSampleTime 断层。
        gaps: u64,
        max_gap: f64,
        last_sample_time: f64,
        /// 断层明细 (发生在第几秒, 跳了多少帧):区分"启动瞬态"与"跑起来之后真的丢帧"。
        /// 只留前 20 条,回调里不做无界增长。
        gap_log: Vec<(f64, f64)>,
        /// 第一次/最后一次回调的墙钟时刻。实测采样率必须**取这两点之间**:从
        /// AudioDeviceStart 起算会把设备启动延迟(实测 ~0.2s)当成"慢了 14000ppm";
        /// 算到停止时刻则会把主循环 200ms 的轮询粒度算进去(370s 上就是 -540ppm 的假象)。
        first_cb: Option<Instant>,
        last_cb: Option<Instant>,
        /// 逐回调两轨帧数不等的次数与最大差值。"单 IOProc 双轨天然等长"这条结论靠它撑,
        /// 只比最终累计长度撑不住(某次少给、下次补回来,累计照样相等)。
        frame_mismatch: u64,
        max_frame_mismatch: u64,
        /// 首回调那一次的帧数不计入速率分母(它对应的是启动前就已入队的那一块)。
        first_frames: u64,
    }

    static STOP: AtomicBool = AtomicBool::new(false);

    unsafe extern "C" fn io_proc(
        _dev: AudioObjectID,
        _now: *const AudioTimeStamp,
        input: *const AudioBufferList,
        input_time: *const AudioTimeStamp,
        _out: *mut AudioBufferList,
        _out_time: *const AudioTimeStamp,
        user: *mut c_void,
    ) -> OSStatus {
        let t0 = Instant::now();
        let s = &mut *(user as *mut Shared);
        if s.first_cb.is_none() {
            s.first_cb = Some(t0);
        }
        if input.is_null() {
            return 0;
        }
        let list = &*input;
        let n = list.mNumberBuffers as usize;
        let bufs = std::slice::from_raw_parts(list.mBuffers.as_ptr(), n);

        // 时间戳连续性:期望本次的 mSampleTime = 上次 + 上次帧数。
        if !input_time.is_null() {
            let ts = &*input_time;
            if ts.mFlags & kAudioTimeStampSampleTimeValid != 0 {
                if s.last_sample_time > 0.0 {
                    let expect = s.last_sample_time;
                    let d = (ts.mSampleTime - expect).abs();
                    if d > 1.0 {
                        s.gaps += 1;
                        if d > s.max_gap {
                            s.max_gap = d;
                        }
                        if s.gap_log.len() < 20 {
                            let at = s.first_cb.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0);
                            s.gap_log.push((at, d));
                        }
                    }
                }
                s.last_sample_time = ts.mSampleTime;
            }
        }

        // buffer 顺序按聚合设备描述里的 subdevices→taps。边界按**累计声道数**判,不是按
        // buffer 序号(Codex P2):多流输入设备(某些 USB 声卡)的 mic 会占好几个 buffer,
        // 按序号判会把第二个 mic buffer 错当成 tap。s.mic_ch = mic 设备的输入声道总数。
        // 同一轨可能横跨**多个 buffer**(多流输入设备):必须按帧合并,不能顺序追加——
        // 追加会让两个 F 帧的 mic buffer 变成 2F 个样本,而 tap 只有 F,轨长直接翻倍
        // (Codex 二轮 P2)。做法:先累加各声道的和(同一帧号累到同一个位置),
        // 循环末尾再按该轨这一回合的总声道数取平均。
        let (mic_base, sys_base) = (s.mic.len(), s.sys.len());
        let (mut mic_frames, mut tap_frames) = (0usize, 0usize);
        let (mut mic_chs, mut tap_chs) = (0usize, 0usize);
        let mut seen_ch = 0usize;
        for b in bufs {
            let ch = b.mNumberChannels as usize;
            if ch == 0 || b.mData.is_null() {
                seen_ch += ch;
                continue;
            }
            let total = (b.mDataByteSize as usize) / std::mem::size_of::<f32>();
            let frames = total / ch;
            let data = std::slice::from_raw_parts(b.mData as *const f32, total);
            let is_mic = seen_ch < s.mic_ch;
            let (base, dst) = if is_mic { (mic_base, &mut s.mic) } else { (sys_base, &mut s.sys) };
            for f in 0..frames {
                let mut acc = 0.0f32;
                for c in 0..ch {
                    acc += data[f * ch + c];
                }
                let idx = base + f;
                if idx < dst.len() {
                    dst[idx] += acc; // 本轨的另一个 buffer 已经写过这一帧,累加
                } else {
                    dst.push(acc);
                }
            }
            if is_mic {
                mic_frames = mic_frames.max(frames);
                mic_chs += ch;
            } else {
                tap_frames = tap_frames.max(frames);
                tap_chs += ch;
            }
            seen_ch += ch;
        }
        // 归一:各自除以本轨这一回合参与的总声道数(单 buffer 时等价于原来的 acc/ch)。
        if mic_chs > 1 {
            for v in &mut s.mic[mic_base..] {
                *v /= mic_chs as f32;
            }
        } else if mic_chs == 1 {
            // 单声道无需归一
        }
        if tap_chs > 1 {
            for v in &mut s.sys[sys_base..] {
                *v /= tap_chs as f32;
            }
        }
        // 逐回调对账(Codex P2):"两轨样本数相等"这条结论必须每次回调都成立才算数。
        // 只在最后比累计长度是不够的——某一次少给、下一次补回来,累计仍然相等。
        let frames_this = mic_frames.max(tap_frames);
        // 逐回调对账要看**合并后**的两轨长度增量,不只是各自 buffer 的帧数——多流合并
        // 万一写歪(某个 buffer 帧数不齐),差值会在这里露出来。
        let (mic_added, sys_added) = (s.mic.len() - mic_base, s.sys.len() - sys_base);
        if mic_added != sys_added || mic_frames != tap_frames {
            s.frame_mismatch += 1;
            let d = (mic_added as i64 - sys_added as i64)
                .unsigned_abs()
                .max((mic_frames as i64 - tap_frames as i64).unsigned_abs());
            if d > s.max_frame_mismatch {
                s.max_frame_mismatch = d;
            }
        }
        s.frames += frames_this as u64;
        if s.callbacks == 0 {
            s.first_frames = frames_this as u64;
        }
        s.last_cb = Some(Instant::now());
        s.callbacks += 1;
        s.cb_us.push(t0.elapsed().as_micros() as u32);
        if s.last_sample_time > 0.0 {
            s.last_sample_time += frames_this as f64;
        }
        let _ = s.src_hz;
        let _ = s.tap_ch;
        0
    }

    pub fn capture(args: &[String]) -> Result<()> {
        let secs: f64 = flag(args, "--secs").and_then(|s| s.parse().ok()).unwrap_or(120.0);
        let out = flag(args, "--out").unwrap_or_else(|| "/tmp/catap".into());
        std::fs::create_dir_all(&out)?;

        let mic = match flag(args, "--input-uid") {
            Some(uid) => all_devices()?
                .into_iter()
                .map(dev_info)
                .find(|d| d.uid == uid)
                .ok_or_else(|| anyhow!("找不到 uid={uid} 的设备(用 list 看)"))?,
            None => default_input()?,
        };
        println!("mic: {} ({}ch @{:.0}Hz)", mic.name, mic.in_channels, mic.nominal_hz);

        let tap = create_tap()?;
        let tf = tap_format(tap.id).unwrap_or_else(|_| unsafe { std::mem::zeroed() });
        println!("tap: uuid={} {:.0}Hz {}ch", tap.uuid, tf.mSampleRate, tf.mChannelsPerFrame);

        let agg = create_aggregate(&mic.uid, &tap.uuid)?;
        let chans =
            buffer_list_channels(&get_prop_bytes(agg.id, &addr(kAudioDevicePropertyStreamConfiguration, kAudioObjectPropertyScopeInput))?);
        let src_hz: f64 =
            get_prop(agg.id, &addr(kAudioDevicePropertyNominalSampleRate, kAudioObjectPropertyScopeGlobal))?;
        println!("聚合设备: id={} 通道布局={chans:?} @{src_hz:.0}Hz", agg.id);
        if chans.len() < 2 {
            bail!("聚合设备只报出 {} 个输入 buffer,拿不到两轨——先跑 probe 看布局", chans.len());
        }
        // 切轨边界取 **mic 设备自己的输入声道总数**,不是第一个 buffer 的声道数:
        // 多流输入设备(部分 USB 声卡)的 mic 会跨多个 buffer(Codex P2)。
        let mic_ch = mic.in_channels as usize;
        let total_ch: usize = chans.iter().map(|c| *c as usize).sum();
        let tap_ch = total_ch.saturating_sub(mic_ch);
        if mic_ch == 0 || tap_ch == 0 {
            bail!("通道账对不上: 聚合设备共 {total_ch}ch,mic 设备声称 {mic_ch}ch → tap 只剩 {tap_ch}ch");
        }
        println!("切轨: 前 {mic_ch}ch 归 mic,其余 {tap_ch}ch 归 tap(聚合共 {total_ch}ch)");

        let cap = (secs * src_hz) as usize + src_hz as usize;
        let mut shared = Box::new(Shared {
            mic_ch,
            tap_ch,
            src_hz,
            mic: Vec::with_capacity(cap),
            sys: Vec::with_capacity(cap),
            callbacks: 0,
            frames: 0,
            cb_us: Vec::with_capacity(secs as usize * 200),
            gaps: 0,
            max_gap: 0.0,
            last_sample_time: 0.0,
            gap_log: Vec::new(),
            first_cb: None,
            last_cb: None,
            first_frames: 0,
            frame_mismatch: 0,
            max_frame_mismatch: 0,
        });

        let mut proc_id: AudioDeviceIOProcID = None;
        let st = unsafe {
            AudioDeviceCreateIOProcID(
                agg.id,
                Some(io_proc),
                &mut *shared as *mut Shared as *mut c_void,
                &mut proc_id,
            )
        };
        if st != 0 {
            bail!("创建 IOProc 失败: {st} ({})", os_status_hint(st));
        }
        // CPU 要报**采集期增量**:进程启动、缓冲预分配、设备建立那几百毫秒 user 时间
        // 与"每秒采集要花多少 CPU"无关,算进去在短跑里会把数字抬高好几倍。
        let cpu_before = process_cpu();
        let started = Instant::now();
        let st = unsafe { AudioDeviceStart(agg.id, proc_id) };
        if st != 0 {
            bail!("启动聚合设备失败: {st} ({})", os_status_hint(st));
        }
        println!("采集中 {secs:.0}s…(此刻请按 scripts/catap-spike.md 的步骤放标定序列)");
        while started.elapsed().as_secs_f64() < secs && !STOP.load(Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        // 停 IOProc 必须确认成功才敢释放回调用的状态(Codex P1):`shared` 是这个函数栈上的
        // Box,函数一返回就析构;若 Stop/Destroy 失败(热插拔测试里完全可能),CoreAudio 仍会
        // 按那个裸指针回调,就是 use-after-free。失败就**故意泄漏**这块状态——探针马上要退出,
        // 泄漏几 MB 无所谓,回调打到已释放内存则是随机崩溃/脏数据。
        let st_stop = unsafe { AudioDeviceStop(agg.id, proc_id) };
        let st_destroy = unsafe { AudioDeviceDestroyIOProcID(agg.id, proc_id) };
        if st_stop != 0 || st_destroy != 0 {
            // 停不下来就**当场放弃这一场**:回调可能还在往 shared 里 push,此后任何读取
            // (统计、落盘)都是与之竞争,任何提前 return(? 号)都会把 Box 析构掉重新
            // 变成 use-after-free。所以立刻 forget 掉状态、直接返回错误,一个字节都不读。
            // 聚合设备/tap 仍由各自的 Drop 拆除(销毁设备本身会终止 IO)。
            std::mem::forget(shared);
            bail!(
                "IOProc 未能确认停止(stop={st_stop} {} / destroy={st_destroy} {}):\
                 本场数据作废,状态已泄漏以免 use-after-free",
                os_status_hint(st_stop),
                os_status_hint(st_destroy)
            );
        }
        let wall = started.elapsed().as_secs_f64();
        // CPU 快照要在**落盘之前**取:写两条几分钟长的 WAV 本身就吃掉可观的 user 时间,
        // 混进去报出来的就不是"采集期的 CPU"了(第一版实测把 0.06% 报成了 0.67%)。
        let cpu_after = process_cpu();
        let (cpu_user, cpu_sys) = (cpu_after.0 - cpu_before.0, cpu_after.1 - cpu_before.1);

        // ── 落盘:两轨都降到 16k mono s16(与 app 的规范轨一致,xcorr_align 直接可读) ──
        // --native:按设备原始率落盘,绕开下面那个无抗混叠的线性抽取。宽带内容
        // (如白噪参考)被 48k→16k 粗抽取一混叠,波形相关性会被毁掉——实测参考对照的
        // corr 只剩 0.1。轨间对比不受影响(两轨走同一条抽取),但与外部参考比就得用 --native。
        let native = args.iter().any(|a| a == "--native");
        let out_hz = if native { src_hz } else { 16_000.0 };
        let mic_path = format!("{out}/mic.wav");
        let sys_path = format!("{out}/system.wav");
        write_wav(&mic_path, &shared.mic, src_hz, out_hz)?;
        write_wav(&sys_path, &shared.sys, src_hz, out_hz)?;

        let mut us = shared.cb_us.clone();
        us.sort_unstable();
        let pick = |q: f64| us.get(((us.len() as f64 - 1.0) * q) as usize).copied().unwrap_or(0);
        let cpu = std::time::Duration::from_micros(us.iter().map(|u| *u as u64).sum());
        // 速率只能量"首回调 → 末回调"这段:两端各有一截与采样无关的时长(启动延迟、
        // 主循环 200ms 轮询粒度),算进去就是几百 ppm 的假象。
        let run = match (shared.first_cb, shared.last_cb) {
            (Some(a), Some(b)) => b.duration_since(a).as_secs_f64(),
            _ => wall,
        };
        let framed = shared.frames.saturating_sub(shared.first_frames) as f64;
        println!("\n== 采集结果 ==");
        println!(
            "墙钟 {wall:.2}s(首→末回调 {run:.2}s)/ 回调 {} 次 / 帧 {}",
            shared.callbacks, shared.frames
        );
        println!(
            "两轨样本数: mic={} system={}(差 {})← 单 IOProc 双轨的结构性保证,不为 0 就说明布局判错了",
            shared.mic.len(),
            shared.sys.len(),
            shared.mic.len() as i64 - shared.sys.len() as i64
        );
        println!(
            "实测源域率: {:.3}Hz(标称 {src_hz:.0}Hz,差 {:+.1}ppm;这是聚合设备时钟相对本机\n\
             \x20            单调墙钟(Instant)的偏差,不是轨间残余漂移——后者要用下面的 xcorr 量)",
            framed / run,
            (framed / run / src_hz - 1.0) * 1e6
        );
        println!("回调耗时 p50={}µs p95={}µs max={}µs", pick(0.5), pick(0.95), pick(1.0));
        // 这是**回调占用**(墙钟),不是 CPU 成本:既不含 coreaudiod 里 tap/聚合设备/drift
        // 重采样的开销,又混进了回调被抢占的时间(Codex P2)。进程自身 CPU 另用 getrusage 报,
        // 系统侧开销要看 coreaudiod,本探针测不到。
        println!(
            "回调占用(墙钟)总计 {:.2}s ≈ 单核 {:.2}%——不等于 CPU 成本,见下",
            cpu.as_secs_f64(),
            cpu.as_secs_f64() / run.max(1e-9) * 100.0
        );
        println!(
            "本进程 CPU(采集期,落盘前快照): user {cpu_user:.2}s + sys {cpu_sys:.2}s = 单核 {:.2}%\
             (不含 coreaudiod 侧的 tap/聚合设备/drift 重采样开销——那要另测)",
            (cpu_user + cpu_sys) / run.max(1e-9) * 100.0
        );
        println!(
            "逐回调两轨帧数不等: {} 次,最大差 {} 帧(结构性保证成立则应为 0)",
            shared.frame_mismatch, shared.max_frame_mismatch
        );
        println!("时间戳断层: {} 次,最大 {:.0} 帧(判据 2:应为 0)", shared.gaps, shared.max_gap);
        for (at, d) in &shared.gap_log {
            println!("  断层 @{at:.1}s 跳 {d:.0} 帧 ≈ {:.0}ms", d * 1000.0 / src_hz);
        }
        println!("轨道电平: mic rms={:.5} peak={:.3} / system rms={:.5} peak={:.3}",
            rms(&shared.mic), peak(&shared.mic), rms(&shared.sys), peak(&shared.sys));
        println!("\n落盘: {mic_path}\n      {sys_path}");
        println!("残余漂移(E1 口径,system 在前):\n  cargo run --bin xcorr_align -- {sys_path} {mic_path}");
        Ok(())
    }

    /// 线性重采样 + s16 落盘。out_hz == src_hz 时是直通(--native)。
    /// spike 不追求抗混叠质量:轨间 xcorr 走同一条抽取、系统性偏差同相抵消;
    /// 但与**外部参考**比对时必须用 --native,否则宽带内容会被混叠毁掉相关性。
    fn write_wav(path: &str, src: &[f32], src_hz: f64, out_hz: f64) -> Result<()> {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: out_hz as u32,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(path, spec)?;
        let ratio = src_hz / out_hz;
        let n_out = if src.is_empty() { 0 } else { ((src.len() as f64) / ratio) as usize };
        for i in 0..n_out {
            let x = i as f64 * ratio;
            let i0 = x.floor() as usize;
            let frac = (x - i0 as f64) as f32;
            let a = src.get(i0).copied().unwrap_or(0.0);
            let b = src.get(i0 + 1).copied().unwrap_or(a);
            let v = a + (b - a) * frac;
            w.write_sample((v.clamp(-1.0, 1.0) * 32767.0) as i16)?;
        }
        w.finalize()?;
        Ok(())
    }

    /// 本进程累计 CPU(user, sys),秒。只算自己这个进程——CoreAudio 的活儿在 coreaudiod 里,
    /// 这里看不见,报数时必须说清楚。
    fn process_cpu() -> (f64, f64) {
        let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
        if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) } != 0 {
            return (0.0, 0.0);
        }
        let s = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
        (s(ru.ru_utime), s(ru.ru_stime))
    }

    fn rms(v: &[f32]) -> f64 {
        if v.is_empty() {
            return 0.0;
        }
        (v.iter().map(|x| (*x as f64) * (*x as f64)).sum::<f64>() / v.len() as f64).sqrt()
    }
    fn peak(v: &[f32]) -> f64 {
        v.iter().fold(0.0f64, |m, x| m.max(x.abs() as f64))
    }

    fn flag(args: &[String], name: &str) -> Option<String> {
        args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
    }
}
