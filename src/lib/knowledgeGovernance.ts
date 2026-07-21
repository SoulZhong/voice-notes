import { listen } from "@tauri-apps/api/event";
import {
  applyKnowledgeOperation,
  mergeEntities,
  splitEntity,
  undoKnowledgeOperation,
  type KnowledgeMutationResult,
  type KnowledgeOperationInput,
  type KnowledgePredicate,
  type MentionEvidence,
  type PendingReviewItem,
  type RelationDetail,
  type SplitEntityRequest,
} from "./knowledge";

export interface GovernanceApi {
  submit(operation: KnowledgeOperationInput): Promise<KnowledgeMutationResult>;
  split(request: SplitEntityRequest): Promise<KnowledgeMutationResult>;
  merge?(sourceId: string, targetId: string): Promise<KnowledgeMutationResult>;
  undo(operationId: string): Promise<KnowledgeMutationResult>;
}

export interface GovernanceController {
  readonly busy: boolean;
  readonly error: string;
  readonly refreshError: string;
  readonly lastOperationId: string | null;
  submit(operation: KnowledgeOperationInput): Promise<KnowledgeMutationResult>;
  split(request: SplitEntityRequest): Promise<KnowledgeMutationResult>;
  merge(sourceId: string, targetId: string): Promise<KnowledgeMutationResult>;
  undo(operationId: string): Promise<KnowledgeMutationResult>;
  retryRefresh(): Promise<void>;
}

export interface GraphIndexStatus {
  state: "building" | "ready" | "error";
  error: string | null;
  stats: Record<string, number> | null;
}

export interface RebuildWaitHandle {
  readonly terminal: Promise<GraphIndexStatus>;
  cancel(): void;
}

export type RebuildStatusListener = (status: GraphIndexStatus) => void;
export type RebuildStatusSubscribe = (
  listener: RebuildStatusListener,
) => Promise<() => void>;
export type PrepareRebuildWait = () => Promise<RebuildWaitHandle>;

export interface GovernanceMention extends MentionEvidence {
  /** Filled by the UI from relation evidence before split preview. */
  relation_ids?: string[];
}

export interface SplitPreview {
  noteCount: number;
  mentionCount: number;
  affectedRelationCount: number;
  selectedMentionIds: string[];
}

export interface PendingGroup {
  key: string;
  label: string;
  items: PendingReviewItem[];
}

export type KnownPendingKind =
  | "invalid_document"
  | "identity_conflict"
  | "stale_evidence"
  | "split_conflict"
  | "relation_review"
  | "time_conflict";

export interface PendingReviewModel {
  kind: KnownPendingKind | "other";
  noteId: string | null;
  relationIds: string[];
  evidenceId: string | null;
  localEntityId: string | null;
  candidateEntityIds: string[];
  message: string | null;
  reason: string | null;
  canConfirm: boolean;
}

export const KNOWLEDGE_CHANGED_EVENT = "knowledge-governance-changed";

export const task10GovernanceApi: GovernanceApi = {
  submit: applyKnowledgeOperation,
  split: splitEntity,
  merge: mergeEntities,
  undo: undoKnowledgeOperation,
};

function errorMessage(error: unknown): string {
  if (error instanceof Error && error.message.trim()) return error.message;
  const message = String(error).trim();
  return message || "未知错误";
}

const subscribeToGraphRebuild: RebuildStatusSubscribe = async (listener) =>
  listen<GraphIndexStatus>("graph_index_status", (event) => listener(event.payload));

/**
 * Installs the rebuild listener before a mutation is sent. Without a backend
 * generation ID, an observed building event is the only safe boundary for
 * accepting the following ready/error as belonging to the queued request.
 */
export async function prepareRebuildWait(
  subscribe: RebuildStatusSubscribe = subscribeToGraphRebuild,
): Promise<RebuildWaitHandle> {
  let sawBuilding = false;
  let settled = false;
  let unlisten: (() => void) | null = null;
  let cleanupPending = false;
  let resolveTerminal!: (status: GraphIndexStatus) => void;

  const cleanup = () => {
    if (cleanupPending) return;
    cleanupPending = true;
    unlisten?.();
  };
  const terminal = new Promise<GraphIndexStatus>((resolve) => {
    resolveTerminal = resolve;
  });
  const listener = (status: GraphIndexStatus) => {
    if (settled) return;
    if (status.state === "building") {
      sawBuilding = true;
      return;
    }
    if (!sawBuilding || (status.state !== "ready" && status.state !== "error")) return;
    settled = true;
    resolveTerminal(status);
    cleanup();
  };

  unlisten = await subscribe(listener);
  if (cleanupPending) unlisten();
  return {
    terminal,
    cancel() {
      if (settled) return;
      settled = true;
      cleanup();
    },
  };
}

export function createGovernanceController(
  api: GovernanceApi,
  refresh: () => Promise<void>,
  prepareWait: PrepareRebuildWait = () => prepareRebuildWait(),
): GovernanceController {
  let busy = false;
  let error = "";
  let refreshError = "";
  let lastOperationId: string | null = null;
  let inFlight: Promise<KnowledgeMutationResult> | null = null;

  function run(mutation: () => Promise<KnowledgeMutationResult>): Promise<KnowledgeMutationResult> {
    if (inFlight) return inFlight;
    if (busy) return Promise.reject(new Error("另一项治理操作正在完成，请稍后重试。"));
    busy = true;
    error = "";
    refreshError = "";
    const current = (async () => {
      let mutationResult: KnowledgeMutationResult;
      let rebuildWait: RebuildWaitHandle | null = null;
      try {
        // Await listener installation before invoking the mutation. This closes
        // the fast-rebuild race where ready could otherwise arrive first.
        rebuildWait = await prepareWait();
        mutationResult = await mutation();
      } catch (cause) {
        rebuildWait?.cancel();
        error = `治理操作未保存：${errorMessage(cause)}。请检查后重试。`;
        throw cause;
      }

      lastOperationId = mutationResult.operation_id;
      if (mutationResult.rebuild_state === "queued") {
        try {
          const terminal = await rebuildWait.terminal;
          if (terminal.state === "error") {
            refreshError = `操作已保存（操作 ID：${mutationResult.operation_id}），但图谱索引重建失败：${terminal.error || "后端未提供详细原因"}。请稍后重试刷新；撤销仍可使用。`;
            return mutationResult;
          }
        } catch (cause) {
          refreshError = `操作已保存（操作 ID：${mutationResult.operation_id}），但等待图谱索引重建失败：${errorMessage(cause)}。请稍后重试刷新；撤销仍可使用。`;
          return mutationResult;
        } finally {
          rebuildWait.cancel();
        }
      } else {
        rebuildWait.cancel();
      }

      // The mutation is already durable. A failed read refresh must not reject
      // the operation or lose the operation ID needed for Undo.
      try {
        await refresh();
      } catch (cause) {
        refreshError = `操作已保存，但图谱刷新失败：${errorMessage(cause)}。可单独重试刷新。`;
      }
      return mutationResult;
    })().finally(() => {
      busy = false;
      inFlight = null;
    });
    inFlight = current;
    return current;
  }

  return {
    get busy() {
      return busy;
    },
    get error() {
      return error;
    },
    get refreshError() {
      return refreshError;
    },
    get lastOperationId() {
      return lastOperationId;
    },
    submit(operation) {
      return run(() => api.submit(operation));
    },
    split(request) {
      return run(() => api.split(request));
    },
    merge(sourceId, targetId) {
      return run(() => {
        if (!api.merge) return Promise.reject(new Error("当前治理 API 不支持实体合并。"));
        return api.merge(sourceId, targetId);
      });
    },
    undo(operationId) {
      return run(() => api.undo(operationId));
    },
    async retryRefresh() {
      if (busy) return;
      busy = true;
      refreshError = "";
      try {
        await refresh();
      } catch (cause) {
        refreshError = `图谱仍未刷新：${errorMessage(cause)}。请稍后再试。`;
        throw cause;
      } finally {
        busy = false;
      }
    },
  };
}

export const buildRenameEntity = (entityId: string, name: string): KnowledgeOperationInput => ({
  kind: "rename_entity",
  payload: { entity_id: entityId, name },
});

export const buildAddAlias = (entityId: string, alias: string): KnowledgeOperationInput => ({
  kind: "add_alias",
  payload: { entity_id: entityId, alias },
});

export const buildRemoveAlias = (entityId: string, alias: string): KnowledgeOperationInput => ({
  kind: "remove_alias",
  payload: { entity_id: entityId, alias },
});

export const buildBindMention = (
  mentionId: string,
  entityId: string,
): KnowledgeOperationInput => ({
  kind: "bind_mention",
  payload: { mention_id: mentionId, entity_id: entityId },
});

/** Person linking is the bind_mention Rust operation with a person entity target. */
export const buildBindPerson = buildBindMention;

export const buildConfirmRelation = (relationId: string): KnowledgeOperationInput => ({
  kind: "confirm_relation",
  payload: { relation_id: relationId },
});

export const buildEditRelation = (
  relationId: string,
  subjectId: string,
  predicate: KnowledgePredicate,
  objectId: string,
  validFrom: string | null,
  validTo: string | null,
  note: string | null,
): KnowledgeOperationInput => ({
  kind: "edit_relation",
  payload: {
    relation_id: relationId,
    subject_id: subjectId,
    predicate: { ...predicate },
    object_id: objectId,
    valid_from: validFrom,
    valid_to: validTo,
    note,
  },
});

export const buildSuppressRelation = (
  subjectId: string,
  predicate: KnowledgePredicate,
  objectId: string,
): KnowledgeOperationInput => ({
  kind: "suppress_relation",
  payload: {
    subject_id: subjectId,
    predicate: { ...predicate },
    object_id: objectId,
  },
});

export const buildEndRelation = (
  relationId: string,
  validTo: string,
): KnowledgeOperationInput => ({
  kind: "end_relation",
  payload: { relation_id: relationId, valid_to: validTo },
});

export const buildRestoreRelation = (operationId: string): KnowledgeOperationInput => ({
  kind: "restore_relation",
  payload: { operation_id: operationId },
});

export const buildCreateEntity = (
  kind: string,
  name: string,
  aliases: string[],
): KnowledgeOperationInput => ({
  kind: "create_entity",
  payload: { kind, name, aliases: [...aliases] },
});

export const buildCreateRelation = (
  subjectId: string,
  predicate: KnowledgePredicate,
  objectId: string,
  validFrom: string | null,
  validTo: string | null,
  note: string | null,
  evidenceIds: string[],
  userAssertion: boolean,
): KnowledgeOperationInput => ({
  kind: "create_relation",
  payload: {
    subject_id: subjectId,
    predicate: { ...predicate },
    object_id: objectId,
    valid_from: validFrom,
    valid_to: validTo,
    note,
    evidence_ids: [...evidenceIds],
    user_assertion: userAssertion,
  },
});

export const buildMergeEntities = (sourceId: string, targetId: string) => ({
  sourceId,
  targetId,
});

export const buildSplitEntity = (
  entityId: string,
  name: string,
  kind: string | null,
  aliases: string[],
  mentionIds: string[],
): SplitEntityRequest => ({
  entity_id: entityId,
  name,
  kind,
  aliases: [...aliases],
  mention_ids: [...mentionIds],
});

export const buildUndo = (operationId: string) => ({ operationId });

export function splitPreview(
  selected: MentionEvidence[],
  total: MentionEvidence[],
): SplitPreview {
  const totalById = new Map<string, GovernanceMention>();
  const orderedTotal = [...total as GovernanceMention[]].sort((left, right) =>
    left.id.localeCompare(right.id) ||
    left.note_id.localeCompare(right.note_id) ||
    left.paragraph_index - right.paragraph_index ||
    left.start_offset - right.start_offset ||
    left.end_offset - right.end_offset ||
    left.quote.localeCompare(right.quote)
  );
  for (const mention of orderedTotal) {
    const previous = totalById.get(mention.id);
    if (!previous) {
      totalById.set(mention.id, { ...mention, relation_ids: [...mention.relation_ids ?? []].sort() });
    } else {
      previous.relation_ids = [...new Set([
        ...previous.relation_ids ?? [],
        ...mention.relation_ids ?? [],
      ])].sort();
    }
  }
  const selectedMentionIds = [...new Set(selected.map((mention) => mention.id))]
    .filter((id) => totalById.has(id))
    .sort();
  const noteIds = new Set<string>();
  const relationIds = new Set<string>();
  for (const id of selectedMentionIds) {
    const mention = totalById.get(id)!;
    noteIds.add(mention.note_id);
    for (const relationId of mention.relation_ids ?? []) relationIds.add(relationId);
  }
  return {
    noteCount: noteIds.size,
    mentionCount: selectedMentionIds.length,
    affectedRelationCount: relationIds.size,
    selectedMentionIds,
  };
}

export function canSubmitSplit(preview: SplitPreview): boolean {
  return preview.mentionCount > 0;
}

const PENDING_GROUPS = [
  { key: "identity_conflict", label: "身份冲突" },
  { key: "relation_review", label: "待确认关系" },
  { key: "time_conflict", label: "时间冲突" },
  { key: "stale_evidence", label: "证据已失效" },
  { key: "split_conflict", label: "拆分冲突" },
  { key: "invalid_document", label: "文档错误" },
  { key: "other", label: "其他" },
] as const;

function payloadRecord(item: PendingReviewItem): Record<string, unknown> {
  return item.payload !== null && typeof item.payload === "object" && !Array.isArray(item.payload)
    ? (item.payload as Record<string, unknown>)
    : {};
}

function stringField(payload: Record<string, unknown>, key: string): string | null {
  const value = payload[key];
  return typeof value === "string" && value.trim() ? value : null;
}

function stringArrayField(payload: Record<string, unknown>, key: string): string[] {
  const value = payload[key];
  if (!Array.isArray(value)) return [];
  return [...new Set(value.filter((item): item is string => typeof item === "string" && Boolean(item.trim())))]
    .sort();
}

export function pendingReviewModel(item: PendingReviewItem): PendingReviewModel {
  const payload = payloadRecord(item);
  const knownKinds: KnownPendingKind[] = [
    "invalid_document",
    "identity_conflict",
    "stale_evidence",
    "split_conflict",
    "relation_review",
    "time_conflict",
  ];
  const kind = knownKinds.includes(item.kind as KnownPendingKind)
    ? item.kind as KnownPendingKind
    : "other";
  const singleRelationId = item.relation_id ?? stringField(payload, "relation_id");
  const relationIds = kind === "time_conflict"
    ? stringArrayField(payload, "relation_ids")
    : singleRelationId ? [singleRelationId] : [];
  return {
    kind,
    noteId: item.note_id ?? stringField(payload, "note_id"),
    relationIds,
    evidenceId: stringField(payload, "evidence_id"),
    localEntityId: stringField(payload, "local_entity_id"),
    candidateEntityIds: stringArrayField(payload, "candidates"),
    message: stringField(payload, "message"),
    reason: stringField(payload, "reason"),
    canConfirm: kind === "relation_review" && relationIds.length === 1,
  };
}

function pendingGroupKey(item: PendingReviewItem): (typeof PENDING_GROUPS)[number]["key"] {
  return pendingReviewModel(item).kind;
}

function comparePending(left: PendingReviewItem, right: PendingReviewItem): number {
  return (
    left.id.localeCompare(right.id) ||
    left.kind.localeCompare(right.kind) ||
    (left.note_id ?? "").localeCompare(right.note_id ?? "")
  );
}

export function groupPending(items: PendingReviewItem[]): PendingGroup[] {
  const buckets = new Map(PENDING_GROUPS.map((group) => [group.key, [] as PendingReviewItem[]]));
  for (const item of items) buckets.get(pendingGroupKey(item))!.push(item);
  return PENDING_GROUPS.flatMap((group) => {
    const groupedItems = buckets.get(group.key)!.map((item) => ({ ...item })).sort(comparePending);
    return groupedItems.length > 0 ? [{ ...group, items: groupedItems }] : [];
  });
}

export function pendingAfterLater(
  items: PendingReviewItem[],
  hiddenIds: ReadonlySet<string>,
): PendingReviewItem[] {
  return items.filter((item) => !hiddenIds.has(item.id));
}

export function retainLastKnownRelation(
  current: RelationDetail | null,
  previous: RelationDetail | null,
): RelationDetail | null {
  return current ?? previous;
}
