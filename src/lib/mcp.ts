// MCP 注册的前端封装。真值源是各 Agent 的配置文件(后端每次现扫),
// 前端不缓存注册状态,操作后重新拉取。
import { invoke } from "@tauri-apps/api/core";

export type AgentStatus = {
  key: string;
  name: string;
  installed: boolean;
  registered: boolean;
  command: string | null;
  stale: boolean;
};

export type RegisterOutcome = { key: string; ok: boolean; error: string | null };

export const mcpAgentsStatus = () => invoke<AgentStatus[]>("mcp_agents_status");
export const mcpRegister = (agents: string[]) => invoke<RegisterOutcome[]>("mcp_register", { agents });
export const mcpUnregister = (agent: string) => invoke<void>("mcp_unregister", { agent });
export const mcpManualSnippet = () => invoke<string>("mcp_manual_snippet");
/** 启动自愈修复数(读即清零,提示条只出一次)。 */
export const mcpHealedCount = () => invoke<number>("mcp_healed_count");

/** Claude Code 技能状态:not_installed | current | stale | unmanaged。 */
export const mcpSkillStatus = () => invoke<string>("mcp_skill_status");
export const mcpSkillInstall = () => invoke<void>("mcp_skill_install");
export const mcpSkillUninstall = () => invoke<void>("mcp_skill_uninstall");

export type ToolCatalogEntry = { name: string; desc: string; gate: "none" | "app" | "control" };
export type CliCatalogEntry = { cmd: string; desc: string };
export type Capabilities = { tools: ToolCatalogEntry[]; cli: CliCatalogEntry[] };

/** `/ai` 页「Agent 能调用什么」清单:MCP 工具 + CLI 命令,纯静态数据。 */
export const mcpCapabilities = () => invoke<Capabilities>("mcp_capabilities");

/** Agent Aing:四家 CLI 的本机探测(key → 可执行路径或 null)。 */
export const refineAgentsProbe = () => invoke<Record<string, string | null>>("refine_agents_probe");

export type SkillRead = { content: string; state: string };

/** 读取 skill 正文;未安装时返回渲染默认稿(state 仍如实为 not_installed)。 */
export const mcpSkillRead = () => invoke<SkillRead>("mcp_skill_read");
/** 保存 = 编辑即接管:剥离受管标记后落盘,保存后状态变 unmanaged。 */
export const mcpSkillSave = (content: string) => invoke<void>("mcp_skill_save", { content });
