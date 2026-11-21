/**
 * todo.ts — the NAI-Lattice shared MIND MAP ledger + quality gate.
 *
 * This is NOT just a TODO list. It is the durable, cross-agent, cross-project
 * "mind map" / breadcrumb knowledge graph that survives any agent's memory loss
 * (context rotation / auto-condensing). Agents in ANY workspace write their
 * durable state here so a future self or another Don can reattach to the same
 * multiverse and continue — the Alice-in-Wonderland continuity contract.
 *
 * Each record is a NODE in that graph and may carry an optional `kind` so it is
 * not forced to look like a chore:
 *   - "task"      — work item (default; the classic todo).
 *   - "decision"  — an architectural/scope decision we agreed on and why.
 *   - "fact"      — a verified durable fact (source + evidence), not a promise.
 *   - "lamp"      — a continuity handoff for a future self / a peer ("read this
 *                   to know where we are and what's next").
 *   - "state"     — a live status / ownership note (who owns which territory).
 *   - "blocker"   — an open impediment (the "blocked" lifecycle lower half).
 * All kinds share the same tree (parent nesting) + lifecycle status so a seminar
 * thread, a decision, a lamp and a task can all live in one navigable mind map.
 *
 * Semantics the CTO asked for:
 *   - "done" can mean: (a) all open items done, OR (b) build was performed (a
 *     gated item), OR (c) deploy was performed (a gated item).
 *   - Work is effectively infinite-scroll: the human keeps appending tasks, so
 *     "done" is a MILESTONE on the CURRENT list, never a terminal state.
 *   - A task is only ever marked done through the quality gate, never by the
 *     agent silently flipping it. `todo_gate` records evidence (what ran, exit,
 *     artifacts, timestamp) and the check consumes that evidence.
 *
 * Shape (mind map): each record has a parent (tree nesting) + a lifecycle.
 *   { id, parent?: id|null, title, kind?: string,
 *     status: open|in_progress|blocked|done,
 *     createdBy, createdAt, doneAt?, updatedAt?, gated?: { needsBuild?, needsDeploy? },
 *     gates?: { build?: {cmd,exit,evidence,doneAt,ok}, deploy?: {...} } }
 *
 * Store: durable JSONL at <package>/state/todo.jsonl, with atomic snapshots.
 * Writers serialize via SQLite and check task revisions before applying changes.
 */
export declare function todoPath(): string;
export type TaskStatus = "open" | "in_progress" | "blocked" | "done";
export type GateKind = "build" | "deploy";
export declare const TASK_DROP_HISTORY = 16;
export interface TaskDrop {
    revision: number;
    from: TaskStatus;
    to: TaskStatus;
    at: string;
    actor?: string;
    workspaceId?: string;
    product?: string;
}
export interface GateRecord {
    kind: GateKind;
    ok: boolean;
    cmd?: string;
    evidence?: string;
    doneAt?: string;
    /** Who verified this gate (the agent / actor label that ran the build/deploy). */
    actor?: string;
    /** The product whose build/deploy this gate attests (the record's owner). */
    product?: string;
    workspaceId?: string;
    /** Non-zero exit captured by the runner when `ok` was false, if available. */
    exit?: number;
}
/**
 * A gate's storage key is per-ATTESTOR so a producer-native build and a
 * consumer deploy never overwrite each other: "${kind}:${attestorProduct}".
 * The task's OWNING product is the product whose gate must be ok for `done`,
 * so `owningGateKey` (attestor = owner) is what the done-check consults.
 */
export type GateKey = `${GateKind}:${string}` | GateKind;
/** The key a gate is stored under, scoped by the PRODUCT THAT RAN the build/deploy. */
export declare function gateKey(attestorProduct: string | undefined, kind: GateKind): GateKey;
/** The key the task's OWNING product's gate lives under (attestor == task owner). */
export declare function owningGateKey(task: Pick<Task, "product">, kind: GateKind): GateKey;
/** True when the gate for `kind` owned by `task.product` is recorded ok.
 *  Backward-compatible: an OLD bare `build`/`deploy` record counts only when its
 *  stamped `product` (if present) matches the task owner, so a pre-per-product
 *  gate recorded by the right owner still unblocks a gated task. */
export declare function owningGateOk(task: Task, kind: GateKind): boolean;
/**
 * A content-addressed artifact attached to a task. Used for repro media that
 * cannot live in a title/description (e.g. a crash repro video, an audio clip,
 * a failing image, a capture log). `sha256` fingerprints the bytes so the
 * recipient can verify what they received (AAF fingerprint discipline). The
 * bytes themselves are NOT copied into the ledger — only the reference/URI
 * (or the caller arranges out-of-band storage for `uri` payloads).
 */
export interface TaskAttachment {
    kind: "video" | "audio" | "image" | "text";
    name: string;
    /** Where the actual bytes are reachable (local path, blob/snapshot uri, etc.). */
    uri?: string;
    /** SHA-256 of the artifact bytes; required for immutable fingerprinting. */
    sha256?: string;
    /** Size in bytes (uncompressed where relevant). */
    size?: number;
    /** Free-form note (what this repro shows, camera/source, timestamps...). */
    note?: string;
}
export interface Task {
    id: string;
    revision?: number;
    drops?: TaskDrop[];
    dropsPrunedThrough?: number;
    workspace?: string;
    workspaceId?: string;
    product?: string;
    assignee?: string;
    updatedBy?: string;
    parent?: string | null;
    title: string;
    /**
     * Full handoff body — the DoD / acceptance criteria, the crash log, the
     * startup/approach ordering, and the "why". Kept OUT of `title` so a title
     * stays a short index and the detail lives here (or in `attachments`).
     */
    description?: string;
    /** Content-addressed repro/evidence artifacts (video/audio/image/text). */
    attachments?: TaskAttachment[];
    /** Node kind in the mind map: "task" (default) | "decision" | "fact" | "lamp" | "state" | "blocker". */
    kind?: string;
    status: TaskStatus;
    createdBy: string;
    createdAt: string;
    updatedAt?: string;
    doneAt?: string;
    gated?: {
        needsBuild?: boolean;
        needsDeploy?: boolean;
    };
    gates?: Partial<Record<GateKey, GateRecord & {
        doneAt: string;
    }>>;
}
export declare function readTodos(strict?: boolean, file?: string): Task[];
type Mutation = {
    ok: true;
    task: Task;
} | {
    ok: false;
    reason: string;
    current?: Task;
};
export declare function withTodoRetry<T>(operation: () => T): Promise<T>;
export declare function writeTodoCompanion<T>(operation: (file: string) => T): Promise<T>;
export declare function persistTask(task: Task): Promise<Mutation>;
export declare function updateTask(id: string, expectedRevision: number | undefined, change: (current: Task) => Task | string): Promise<Mutation>;
export declare function upsertTask(updated: Task): Task | null;
export type DoneCheck = {
    done: true;
    milestone: "all_items" | "build_done" | "deploy_done";
    commitChecklist: string[];
    summary: Summary;
} | {
    done: false;
    reason: "open_remaining" | "needs_build" | "needs_deploy" | "blocked";
    summary: Summary;
    incomplete: Task[];
};
export interface Summary {
    total: number;
    open: number;
    in_progress: number;
    blocked: number;
    done: number;
    doneAt?: string;
}
/**
 * The CTO-approved "done" rule:
 *   A list is DONE when every task is `done` (all_items),
 *   OR the still-open tasks are only those whose quality gate is satisfied
 *   (needsBuild => build gate ok recorded, needsDeploy => deploy gate ok recorded).
 * A `blocked` task always blocks done.
 */
export declare function doneCheck(now?: string, todos?: Task[]): DoneCheck;
/** The exact evidence-bearing commit message + checklist an agent/host commits. */
export declare function commitChecklist(todos: Task[], milestone: string): string[];
export declare function newTask(title: string, opts?: Partial<Task>): Task;
export {};
