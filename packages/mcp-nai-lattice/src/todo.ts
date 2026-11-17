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

import { readFileSync, writeFileSync, mkdirSync, readdirSync, statSync } from "node:fs"
import { dirname, join } from "node:path"
import { fileURLToPath } from "node:url"
import { randomUUID } from "node:crypto"

const __dirname = dirname(fileURLToPath(import.meta.url))
// Lazy: read env per call so a test/host can point the ledger at an isolated path
// without re-importing (matches REGISTRY_PATH behavior in index.ts).
function todoPath(): string {
  return process.env.TODO_PATH ?? join(__dirname, "..", "state", "todo.jsonl")
}

export type TaskStatus = "open" | "in_progress" | "blocked" | "done"
export type GateKind = "build" | "deploy"

export interface GateRecord {
  kind: GateKind
  ok: boolean
  cmd?: string
  evidence?: string
  doneAt?: string
}

export interface Task {
  id: string
  parent?: string | null
  title: string
  status: TaskStatus
  createdBy: string
  createdAt: string
  updatedAt?: string
  doneAt?: string
  gated?: { needsBuild?: boolean; needsDeploy?: boolean }
  gates?: Partial<Record<GateKind, GateRecord & { doneAt: string }>>
}

// ---------------------------------------------------------------------------
// Store (append-only, bounded)
// ---------------------------------------------------------------------------
const MAX_TODO_BYTES = 4 * 1024 * 1024

function ensureDir() {
  mkdirSync(dirname(todoPath()), { recursive: true })
}

export function readTodos(): Task[] {
  try {
    const raw = readFileSync(todoPath(), "utf8")
    return raw
      .split("\n")
      .filter(Boolean)
      .map((line) => {
        try {
          return JSON.parse(line) as Task
        } catch {
          return null // skip corrupt line — never break the whole ledger
        }
      })
      .filter((t): t is Task => !!t && typeof t.id === "string" && !!t.title)
  } catch {
    return []
  }
}

function sizeOk(): boolean {
  try {
    return statSync(todoPath()).size <= MAX_TODO_BYTES
  } catch {
    return true
  }
}

function persist(append: Task): { ok: boolean; task?: Task; reason?: string } {
  ensureDir()
  if (!sizeOk()) return { ok: false, reason: "todo ledger cap reached" }
  try {
    writeFileSync(todoPath(), JSON.stringify(append) + "\n", { flag: "a" })
    return { ok: true, task: append }
  } catch (e) {
    return { ok: false, reason: String((e as Error)?.message ?? e) }
  }
}
export { persist as persistTask }

/** In-place update of a task by id (append a new revision with updatedAt). */
export function upsertTask(updated: Task): Task | null {
  const todos = readTodos()
  const idx = todos.findIndex((t) => t.id === updated.id)
  const rev: Task = { ...(updated as Task), updatedAt: new Date().toISOString() }
  if (idx >= 0) {
    todos[idx] = rev
  } else {
    todos.push(rev)
  }
  // rewrite whole ledger (bounded, small in practice, atomic rename on same volume)
  ensureDir()
  try {
    writeFileSync(todoPath(), todos.map((t) => JSON.stringify(t)).join("\n") + "\n", "utf8")
    return rev
  } catch {
    return null
  }
}

// ---------------------------------------------------------------------------
// Quality gate — the "done" determination
// ---------------------------------------------------------------------------
export type DoneCheck =
  | { done: true; milestone: "all_items" | "build_done" | "deploy_done"; commitChecklist: string[]; summary: Summary }
  | { done: false; reason: "open_remaining" | "needs_build" | "needs_deploy" | "blocked"; summary: Summary; incomplete: Task[] }

export interface Summary {
  total: number
  open: number
  in_progress: number
  blocked: number
  done: number
  doneAt?: string
}

/**
 * The CTO-approved "done" rule:
 *   A list is DONE when every task is `done` (all_items),
 *   OR the still-open tasks are only those whose quality gate is satisfied
 *   (needsBuild => build gate ok recorded, needsDeploy => deploy gate ok recorded).
 * A `blocked` task always blocks done.
 */
export function doneCheck(now = new Date().toISOString()): DoneCheck {
  const todos = readTodos()
  const summary: Summary = {
    total: todos.length,
    open: todos.filter((t) => t.status === "open").length,
    in_progress: todos.filter((t) => t.status === "in_progress").length,
    blocked: todos.filter((t) => t.status === "blocked").length,
    done: todos.filter((t) => t.status === "done").length,
  }

  const blocked = todos.filter((t) => t.status === "blocked")
  if (blocked.length) {
    return { done: false, reason: "blocked", summary, incomplete: blocked }
  }

  const notDone = todos.filter((t) => t.status !== "done")
  if (notDone.length === 0) {
    summary.doneAt = now
    return { done: true, milestone: "all_items", summary, commitChecklist: commitChecklist(todos, "all_items") }
  }

  // Everything remaining is open/in_progress. Decide by gates:
  const needsBuild = notDone.filter((t) => t.gated?.needsBuild)
  const needsDeploy = notDone.filter((t) => t.gated?.needsDeploy)
  const ungated = notDone.filter((t) => !t.gated?.needsBuild && !t.gated?.needsDeploy)

  // Prefer the most specific milestone explanation.
  const buildUnsatisfied = needsBuild.filter((t) => t.gates?.build?.ok !== true)
  const deployUnsatisfied = needsDeploy.filter((t) => t.gates?.deploy?.ok !== true)

  if (ungated.length || buildUnsatisfied.length || deployUnsatisfied.length) {
    const incomplete = [
      ...ungated,
      ...buildUnsatisfied,
      ...deployUnsatisfied,
    ]
    return { done: false, reason: "open_remaining", summary, incomplete }
  }

  // All remaining tasks are gated and their gates are satisfied.
  if (deployUnsatisfied.length === 0 && needsDeploy.length) {
    return { done: true, milestone: "deploy_done", summary, commitChecklist: commitChecklist(todos, "deploy_done") }
  }
  return { done: true, milestone: "build_done", summary, commitChecklist: commitChecklist(todos, "build_done") }
}

/** The exact evidence-bearing commit message + checklist an agent/host commits. */
export function commitChecklist(todos: Task[], milestone: string): string[] {
  const done = todos.filter((t) => t.status === "done" || t.doneAt)
  const lines = [
    `milestone: ${milestone}`,
    `scope: current NAI-Lattice todo list`,
    `tasks: ${todos.length} total, ${done.length} done`,
  ]
  for (const t of done) {
    lines.push(
      `- [${t.doneAt ? "x" : " "}] ${t.title}` + (t.doneAt ? ` (finished ${t.doneAt})` : ""),
    )
  }
  // add gate evidence
  for (const t of todos) {
    if (t.gates?.build?.ok) lines.push(`- build gate ok: ${t.title} @ ${t.gates.build.doneAt} ${mkCmd(t.gates.build.cmd)}`)
    if (t.gates?.deploy?.ok) lines.push(`- deploy gate ok: ${t.title} @ ${t.gates.deploy.doneAt} ${mkCmd(t.gates.deploy.cmd)}`)
  }
  lines.push(`completed-at: ${new Date().toISOString()}`)
  return lines
}
function mkCmd(c?: string): string {
  return c && c.length ? "(" + c + ")" : ""
}

// ---------------------------------------------------------------------------
// Task helpers
// ---------------------------------------------------------------------------
export function newTask(title: string, opts: Partial<Task> = {}): Task {
  const now = new Date().toISOString()
  return {
    id: opts.id ?? randomUUID(),
    parent: opts.parent ?? null,
    title,
    status: opts.status ?? "open",
    createdBy: opts.createdBy ?? "agent",
    createdAt: opts.createdAt ?? now,
    gated: opts.gated,
    gates: opts.gates,
  }
}