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
    cc::Build::new()
        .cpp(true)
        .std("c++17")
        .file("cxx/sherpa_barrier.cc")
        .compile("sherpa_barrier");
    println!("cargo:rerun-if-changed=cxx/sherpa_barrier.cc");
    tauri_build::build()
}
