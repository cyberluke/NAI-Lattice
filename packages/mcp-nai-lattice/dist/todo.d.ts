/**
 * todo.ts — the NAI-Lattice shared mind-map todo ledger + quality gate.
 *
 * A cross-agent, cross-project "mind map" todo list owned by the lattice (the
 * same way projects.json is owned). Agents DO NOT hunt for a per-repo TODO file:
 * the authoritative list is this ledger, read/written through the MCP tools.
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
 *   { id, parent?: id|null, title, status: open|in_progress|blocked|done,
 *     createdBy, createdAt, doneAt?, updatedAt?, gated?: { needsBuild?, needsDeploy? },
 *     gates?: { build?: {cmd,exit,evidence,doneAt,ok}, deploy?: {...} } }
 *
 * Store: durable append-only JSONL at <package>/state/todo.jsonl (like projects.json).
 * Writers serialize on a process-owned lock (reused pattern from the hardened satin gate).
 */
export type TaskStatus = "open" | "in_progress" | "blocked" | "done";
export type GateKind = "build" | "deploy";
export interface GateRecord {
    kind: GateKind;
    ok: boolean;
    cmd?: string;
    evidence?: string;
    doneAt?: string;
}
export interface Task {
    id: string;
    parent?: string | null;
    title: string;
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
export declare function readTodos(): Task[];
declare function persist(append: Task): {
    ok: boolean;
    task?: Task;
    reason?: string;
};
export { persist as persistTask };
/** In-place update of a task by id (append a new revision with updatedAt). */
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
export declare function doneCheck(now?: string): DoneCheck;
/** The exact evidence-bearing commit message + checklist an agent/host commits. */
export declare function commitChecklist(todos: Task[], milestone: string): string[];
export declare function newTask(title: string, opts?: Partial<Task>): Task;
