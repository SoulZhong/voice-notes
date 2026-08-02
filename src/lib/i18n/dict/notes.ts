import type { Dict, Msg } from "../types";

// notes 领域文案分片。键一律以 "notes." 前缀命名,分片之间不得重键(有测试哨兵)。
export const zh = {} as const satisfies Dict;
export const en = {} satisfies Record<keyof typeof zh, Msg>;
