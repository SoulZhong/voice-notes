import { spawnSync } from "node:child_process";

// Windows 曾在此暂存 sherpa-onnx/onnxruntime 运行时 DLL(sherpa-rs 动态链接时代)。
// 迁移到官方 sherpa-onnx crate 后默认静态链接(含 onnxruntime),Windows 不再有
// 需要随包分发的原生 DLL,此步骤只剩 macOS 的 abseil 处理(webrtc-audio-processing
// 链接 Homebrew abseil,与 sherpa 无关)。
if (process.platform !== "darwin") {
  console.log("fix-bundle-dylibs: skipped (no runtime libs to stage on this platform)");
  process.exit(0);
}

const result = spawnSync("bash", ["src-tauri/fix-bundle-dylibs.sh"], {
  stdio: "inherit",
});

if (result.error) {
  console.error(`fix-bundle-dylibs: failed to start bash: ${result.error.message}`);
  process.exit(1);
}

process.exit(result.status ?? 1);
