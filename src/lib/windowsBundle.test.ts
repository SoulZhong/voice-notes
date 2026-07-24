import { describe, expect, it } from 'vitest';
import windowsBundleConfig from '../../src-tauri/tauri.windows.conf.json?raw';

const REQUIRED_WINDOWS_DLLS = [
  'cargs.dll',
  'onnxruntime.dll',
  'onnxruntime_providers_shared.dll',
  'sherpa-onnx-c-api.dll',
  'sherpa-onnx-cxx-api.dll'
];

describe('Windows installer resources', () => {
  it('places every native runtime DLL next to voice-notes.exe', () => {
    const config = JSON.parse(windowsBundleConfig);
    const resources = config.bundle.resources as Record<string, string>;

    for (const dll of REQUIRED_WINDOWS_DLLS) {
      expect(resources[`target/bundle-libs/windows/${dll}`], dll).toBe('');
    }
  });
});
