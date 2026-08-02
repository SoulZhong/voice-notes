import type { Dict, Msg } from "../types";

// governance 领域文案分片。键一律以 "governance." 前缀命名,分片之间不得重键(有测试哨兵)。
export const zh = {} as const satisfies Dict;
export const en = {} satisfies Record<keyof typeof zh, Msg>;
