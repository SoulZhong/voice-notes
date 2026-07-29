import { describe, expect, it } from 'vitest';
import windowsBundleConfig from '../../src-tauri/tauri.windows.conf.json?raw';

describe('Windows installer resources', () => {
  it('does not bundle obsolete runtime DLLs for the statically linked ASR engine', () => {
    const config = JSON.parse(windowsBundleConfig);
    const resources = config.bundle.resources as Record<string, string>;

    expect(resources).toEqual({});
  });
});
