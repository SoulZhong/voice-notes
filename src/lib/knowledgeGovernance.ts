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

export function createGovernanceController(
  api: GovernanceApi,
  refresh: () => Promise<void>,
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
      try {
        mutationResult = await mutation();
      } catch (cause) {
        error = `治理操作未保存：${errorMessage(cause)}。请检查后重试。`;
        throw cause;
      }

      // The mutation is already durable at this point. A failed read refresh must not
      // reject the successful mutation or lose the operation ID needed for Undo.
      lastOperationId = mutationResult.operation_id;
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
  { key: "identity_candidate", label: "疑似重复或人物匹配" },
  { key: "low_confidence", label: "低置信关系" },
  { key: "custom_predicate", label: "自定义关系类型" },
  { key: "time_conflict", label: "时间冲突" },
  { key: "identity_conflict", label: "身份冲突" },
  { key: "other", label: "其他" },
] as const;

function payloadRecord(item: PendingReviewItem): Record<string, unknown> {
  return item.payload !== null && typeof item.payload === "object" && !Array.isArray(item.payload)
    ? (item.payload as Record<string, unknown>)
    : {};
}

function pendingGroupKey(item: PendingReviewItem): (typeof PENDING_GROUPS)[number]["key"] {
  const kind = item.kind.toLowerCase();
  const payload = payloadRecord(item);
  if (
    kind.includes("duplicate") ||
    kind.includes("person_candidate") ||
    kind.includes("person_match") ||
    kind === "identity_candidate"
  ) return "identity_candidate";
  if (kind.includes("custom_predicate")) return "custom_predicate";
  if (kind.includes("low_confidence")) return "low_confidence";
  if (kind === "relation_review") {
    return payload.predicate_type === "custom" || payload.predicate === "custom"
      ? "custom_predicate"
      : "low_confidence";
  }
  if (kind.includes("time_conflict")) return "time_conflict";
  if (
    kind === "identity_conflict" &&
    ((Array.isArray(payload.candidates) && payload.candidates.length > 0) ||
      (typeof payload.reason === "string" && /duplicate|candidate|person|人物|候选|重复/i.test(payload.reason)))
  ) return "identity_candidate";
  if (
    kind.includes("identity_conflict") ||
    kind === "split_conflict" ||
    kind === "stale_evidence"
  ) return "identity_conflict";
  return "other";
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
