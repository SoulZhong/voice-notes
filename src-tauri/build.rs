fn main() {
    // screencapturekit 牌内部链接 Swift 垫片，其中 libswift_Concurrency 以 @rpath 引用。
    // 依赖包 build.rs 里的 cargo:rustc-link-arg 不会传递给下游二进制（cargo 限制），
    // 所以本包的 test/app 二进制必须自己补 Swift 运行时的 rpath，否则 dyld 启动即崩。
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
        // 打包后的 .app 里 abseil dylib(webrtc-audio-processing 依赖;sherpa/
        // onnxruntime 已静态链接)放在 Contents/Frameworks(见 tauri.conf.json
        // bundle.macOS.frameworks),
        // 二进制须带这条 rpath 才能在用户机器上找到它们;dev 模式下该路径
        // 不存在,dyld 会继续走 cargo 注入的 DYLD_FALLBACK_LIBRARY_PATH,无害。
        println!("cargo:rustc-link-arg=-Wl,-rpath,@executable_path/../Frameworks");
    }
    // issue #98:sherpa C API 无异常防护,onnxruntime 的 C++ 异常穿 FFI 会让 Rust
    // abort。cxx/sherpa_barrier.cc 在 C++ 侧包 try/catch;SherpaOnnx* 符号由既有
    // sherpa 静态库在最终链接期解析(shim 自声明原型,不依赖其头文件)。
    let mut barrier = cc::Build::new();
    barrier.cpp(true).std("c++17").file("cxx/sherpa_barrier.cc");
    // Windows:sherpa 预编译静态库为 /MT(static CRT),cc 默认 /MD 会触发对象级
    // LNK2038 RuntimeLibrary 硬冲突(CI 实证)——shim 必须同为 /MT。
    // /EHs(Codex P1):cc 在 MSVC 不加任何 /EH 标志,旧式 EH 下 catch(...) 会连
    // SEH 结构化异常(访问违例)一起吞掉,损坏态继续跑比 abort 更糟;/EHs 限定
    // 只接同步 C++ 异常且全展开。刻意不用 /EHsc:/EHc 假定 extern "C" 不抛,
    // 而本屏障的前提恰是 sherpa 的 extern "C" 会抛 C++ 异常,/EHc 会废掉 try/catch。
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        barrier.static_crt(true);
        barrier.flag("/EHs");
    }
    barrier.compile("sherpa_barrier");
    println!("cargo:rerun-if-changed=cxx/sherpa_barrier.cc");
    build_apple_asr();
    tauri_build::build()
}

/// Apple SpeechTranscriber 桥(swift/apple_asr.swift)。编它要 macOS 26 SDK;SDK 更老
/// (旧 Xcode 的 CI 机)时不编,也不发 cfg,Rust 侧 asr::apple 落到「系统不支持」桩,
/// 构建照常通过。部署目标仍是 13.0:新 API 全在 #available 后面,旧系统运行不受影响。
fn build_apple_asr() {
    println!("cargo:rustc-check-cfg=cfg(apple_asr)");
    println!("cargo:rerun-if-changed=swift/apple_asr.swift");
    println!("cargo:rerun-if-env-changed=VN_NO_APPLE_ASR");
    println!("cargo:rerun-if-env-changed=VN_REQUIRE_APPLE_ASR");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") || std::env::var_os("VN_NO_APPLE_ASR").is_some() {
        return;
    }
    let xcrun = |args: &[&str]| -> Option<String> {
        let out = std::process::Command::new("xcrun").args(args).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    let sdk_major = xcrun(&["--sdk", "macosx", "--show-sdk-version"])
        .and_then(|v| v.split('.').next().and_then(|m| m.parse::<u32>().ok()))
        .unwrap_or(0);
    if sdk_major < 26 {
        // 发版构建设 VN_REQUIRE_APPLE_ASR=1:宁可构建失败,也不能悄悄发出一个少了省电引擎的版本。
        let required = std::env::var("VN_REQUIRE_APPLE_ASR").is_ok_and(|v| !v.is_empty());
        assert!(!required, "VN_REQUIRE_APPLE_ASR:需要 macOS 26 SDK(Xcode 26+),当前 SDK {sdk_major}");
        println!("cargo:warning=macOS SDK {sdk_major} < 26,跳过 Apple 语音识别桥(该引擎在本构建中不可用)");
        return;
    }
    let arch = match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("aarch64") => "arm64",
        Ok("x86_64") => "x86_64",
        other => panic!("apple_asr: 不支持的架构 {other:?}"),
    };
    let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let lib = out_dir.join("libapple_asr.a");
    let status = std::process::Command::new("xcrun")
        .args(["--sdk", "macosx", "swiftc", "-emit-library", "-static", "-parse-as-library", "-O"])
        .args(["-swift-version", "5", "-module-name", "AppleAsr"])
        .args(["-target", &format!("{arch}-apple-macos13.0")])
        .arg("-o")
        .arg(&lib)
        .arg("swift/apple_asr.swift")
        .status()
        .expect("apple_asr: 无法启动 swiftc");
    assert!(status.success(), "apple_asr: swiftc 编译失败");

    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=static=apple_asr");
    for fw in ["Speech", "AVFoundation", "CoreMedia", "Foundation"] {
        println!("cargo:rustc-link-lib=framework={fw}");
    }
    // Swift 静态库引用的运行时(含为旧部署目标准备的兼容垫片库)要能被链接器找到。
    if let Some(sdk) = xcrun(&["--sdk", "macosx", "--show-sdk-path"]) {
        println!("cargo:rustc-link-search=native={sdk}/usr/lib/swift");
    }
    if let Some(swiftc) = xcrun(&["--find", "swiftc"]) {
        let toolchain = std::path::Path::new(&swiftc).parent().and_then(|p| p.parent()).unwrap();
        println!("cargo:rustc-link-search=native={}/lib/swift/macosx", toolchain.display());
    }
    println!("cargo:rustc-cfg=apple_asr");
}
