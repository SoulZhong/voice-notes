// 字典条目:静态文案用字符串(支持 {name} 占位插值),英文复数等需要逻辑的用函数。
export type Msg = string | ((p: Record<string, unknown>) => string);
export type Dict = Record<string, Msg>;
