import { invoke } from "@tauri-apps/api/core";
import type { EdgeRow, EntitySummary } from "./graph";

export type RelationStatus = "current" | "historical";
export type RelationOrigin = "model" | "confirmed" | "manual" | "user_assertion";
export type KnowledgeRebuildState = "queued";
export type BackfillProvider = "openai" | "agent";
export type BackfillState = "running" | "completed" | "cancelled" | "failed";
export type JsonValue =
  | null
  | boolean
  | number
  | string
  | JsonValue[]
  | { [key: string]: JsonValue };

export interface SemanticEdge {
  id: string;
  subject_id: string;
  object_id: string;
  predicate_type: string;
  predicate_label: string | null;
  status: RelationStatus;
  confidence: number;
  origin: RelationOrigin;
  evidence_count: number;
  note_count: number;
  valid_from: string | null;
  valid_to: string | null;
}

export interface SemanticGraphData {
  nodes: EntitySummary[];
  semantic_edges: SemanticEdge[];
  cooccurrence_edges: EdgeRow[];
  degraded: boolean;
  message: string | null;
}

export interface KnowledgeFilter {
  entity_kinds: string[];
  predicate_types: string[];
  from: string | null;
  to: string | null;
  include_history: boolean;
  include_cooccurrence: boolean;
}

export interface SemanticEntityDetail {
  id: string;
  kind: string;
  name: string;
  aliases: string[];
  confirmed: boolean;
  is_person: boolean;
  note_count: number;
  mention_total: number;
  relations: SemanticEdge[];
  degraded: boolean;
  message: string | null;
}

export interface RelationEvidence {
  id: string;
  note_id: string;
  paragraph_index: number;
  start_offset: number;
  end_offset: number;
  quote: string;
  source_seqs: number[];
  source_hash: string;
  subject_mentions: string[];
  object_mentions: string[];
}

export interface RelationDetail {
  relation: SemanticEdge;
  provider: string | null;
  model: string | null;
  note_ids: string[];
  evidence: RelationEvidence[];
}

export interface PendingReviewItem {
  id: string;
  kind: string;
  note_id: string | null;
  relation_id: string | null;
  payload: JsonValue;
}

export interface MentionEvidence {
  id: string;
  note_id: string;
  entity_id: string;
  paragraph_index: number;
  start_offset: number;
  end_offset: number;
  quote: string;
}

export type KnowledgePathDirection = "forward" | "reverse";
export type KnowledgePathOrigin = RelationOrigin | "cooccurrence";

export interface KnowledgePathStep {
  id: string;
  from_id: string;
  to_id: string;
  subject_id: string;
  object_id: string;
  predicate_type: string;
  predicate_label: string | null;
  direction: KnowledgePathDirection;
  origin: KnowledgePathOrigin;
  confidence: number;
  evidence_count: number;
  note_count: number;
}

export interface KnowledgePath {
  entity_ids: string[];
  steps: KnowledgePathStep[];
  total_cost: number;
}

/** Mirrors store::RelationPredicate when it crosses a mutation IPC boundary. */
export interface KnowledgePredicate {
  type: string;
  label: string | null;
}

type Operation<K extends string, P> = { kind: K; payload: P };

export type KnowledgeOperationInput =
  | Operation<"rename_entity", { entity_id: string; name: string }>
  | Operation<"add_alias", { entity_id: string; alias: string }>
  | Operation<"remove_alias", { entity_id: string; alias: string }>
  | Operation<"bind_mention", { mention_id: string; entity_id: string }>
  | Operation<"confirm_relation", { relation_id: string }>
  | Operation<
      "edit_relation",
      {
        relation_id: string;
        subject_id: string;
        predicate: KnowledgePredicate;
        object_id: string;
        valid_from: string | null;
        valid_to: string | null;
        note: string | null;
      }
    >
  | Operation<
      "suppress_relation",
      { subject_id: string; predicate: KnowledgePredicate; object_id: string }
    >
  | Operation<"end_relation", { relation_id: string; valid_to: string }>
  | Operation<"restore_relation", { operation_id: string }>
  | Operation<"create_entity", { kind: string; name: string; aliases: string[] }>
  | Operation<
      "create_relation",
      {
        subject_id: string;
        predicate: KnowledgePredicate;
        object_id: string;
        valid_from: string | null;
        valid_to: string | null;
        note: string | null;
        evidence_ids: string[];
        user_assertion: boolean;
      }
    >;

export interface SplitEntityRequest {
  entity_id: string;
  name: string;
  kind: string | null;
  aliases: string[];
  mention_ids: string[];
}

export interface KnowledgeMutationResult {
  operation_id: string;
  entity_id: string | null;
  rebuild_state: KnowledgeRebuildState;
  rebuild_generation: number | null;
}

export interface BackfillRequest {
  note_ids: string[] | null;
  provider: BackfillProvider;
}

export interface BackfillPreview {
  note_ids: string[];
  provider: BackfillProvider;
  model: string;
  contract_version: number;
}

export interface BackfillFailure {
  note_id: string;
  error: string;
}

export interface BackfillProgress {
  state: BackfillState;
  completed: number;
  total: number;
  current_note_id: string | null;
  failed: BackfillFailure[];
}

export const semanticGraph = (filter: KnowledgeFilter) =>
  invoke<SemanticGraphData>("semantic_graph", { filter });

export const semanticEntityDetail = (entityId: string, filter: KnowledgeFilter) =>
  invoke<SemanticEntityDetail | null>("semantic_entity_detail", { entityId, filter });

export const relationDetail = (relationId: string) =>
  invoke<RelationDetail | null>("relation_detail", { relationId });

export const pendingReview = (filter: KnowledgeFilter) =>
  invoke<PendingReviewItem[]>("pending_review", { filter });

export const entityMentions = (entityId: string) =>
  invoke<MentionEvidence[]>("entity_mentions", { entityId });

export const knowledgePath = (start: string, end: string, filter: KnowledgeFilter) =>
  invoke<KnowledgePath | null>("shortest_path", { start, end, filter });

export const applyKnowledgeOperation = (operation: KnowledgeOperationInput) =>
  invoke<KnowledgeMutationResult>("apply_knowledge_operation", { operation });

export const splitEntity = (request: SplitEntityRequest) =>
  invoke<KnowledgeMutationResult>("split_entity", { request });

export const mergeEntities = (sourceId: string, targetId: string) =>
  invoke<KnowledgeMutationResult>("merge_entities", { sourceId, targetId });

export const undoKnowledgeOperation = (operationId: string) =>
  invoke<KnowledgeMutationResult>("undo_knowledge_operation", { operationId });

export const previewRelationBackfill = (noteIds?: string[]) =>
  invoke<BackfillPreview>("preview_relation_backfill", { noteIds: noteIds ?? null });

export const startRelationBackfill = (request: BackfillRequest) =>
  invoke<void>("start_relation_backfill", { request });

export const cancelRelationBackfill = () => invoke<void>("cancel_relation_backfill");
