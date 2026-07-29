import { spawnSync } from "node:child_process";

// Windows 曾在此暂存 sherpa-onnx/onnxruntime 运行时 DLL(sherpa-rs 动态链接时代)。
// 迁移到官方 sherpa-onnx crate 后默认静态链接(含 onnxruntime),Windows 不再有
// 需要随包分发的原生 DLL。macOS 的 abseil 也已改为 meson wrap 静态内嵌
// (2026-07-29),此步骤只剩守卫职责:主二进制不得引用任何 Homebrew 动态库。
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
