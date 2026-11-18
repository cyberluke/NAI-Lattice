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
    gates?: Partial<Record<GateKind, GateRecord & {
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
