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
    tauri_build::build()
}
