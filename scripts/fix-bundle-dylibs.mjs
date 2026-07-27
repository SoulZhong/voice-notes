import { spawnSync } from "node:child_process";
import { copyFileSync, existsSync, mkdirSync } from "node:fs";
import { isAbsolute, resolve } from "node:path";

if (process.platform === "win32") {
  const runtimeDlls = [
    "cargs.dll",
    "onnxruntime.dll",
    "onnxruntime_providers_shared.dll",
    "sherpa-onnx-c-api.dll",
    "sherpa-onnx-cxx-api.dll",
  ];
  const configuredTarget = process.env.CARGO_TARGET_DIR;
  const targetRoots = configuredTarget
    ? [
        isAbsolute(configuredTarget)
          ? configuredTarget
          : resolve(process.cwd(), configuredTarget),
        resolve(process.cwd(), "src-tauri", configuredTarget),
      ]
    : [resolve(process.cwd(), "src-tauri", "target")];
  const releaseDir = targetRoots
    .map((root) => resolve(root, "release"))
    .find((candidate) => runtimeDlls.every((dll) => existsSync(resolve(candidate, dll))));

  if (!releaseDir) {
    console.error(
      `prepare-windows-runtime: missing native DLLs in ${targetRoots.join(", ")}`,
    );
    process.exit(1);
  }

  const bundleDir = resolve(
    process.cwd(),
    "src-tauri",
    "target",
    "bundle-libs",
    "windows",
  );
  mkdirSync(bundleDir, { recursive: true });
  for (const dll of runtimeDlls) {
    copyFileSync(resolve(releaseDir, dll), resolve(bundleDir, dll));
  }
  console.log(`prepare-windows-runtime: copied ${runtimeDlls.length} DLLs`);
  process.exit(0);
}

if (process.platform !== "darwin") {
  console.log("fix-bundle-dylibs: skipped (unsupported platform)");
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
