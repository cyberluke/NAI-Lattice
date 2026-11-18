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

import { readFileSync, writeFileSync, mkdirSync, renameSync, rmSync } from "node:fs"
import { createRequire } from "node:module"
import { dirname, join } from "node:path"
import { fileURLToPath } from "node:url"
import { randomUUID } from "node:crypto"

const __dirname = dirname(fileURLToPath(import.meta.url))
const require = createRequire(import.meta.url)
// Lazy: read env per call so a test/host can point the ledger at an isolated path
// without re-importing (matches REGISTRY_PATH behavior in index.ts).
export function todoPath(): string {
  return process.env.TODO_PATH ?? join(__dirname, "..", "state", "todo.jsonl")
}

export type TaskStatus = "open" | "in_progress" | "blocked" | "done"
export type GateKind = "build" | "deploy"
export const TASK_DROP_HISTORY = 16

export interface TaskDrop {
  revision: number
  from: TaskStatus
  to: TaskStatus
  at: string
  actor?: string
  workspaceId?: string
  product?: string
}

export interface GateRecord {
  kind: GateKind
  ok: boolean
  cmd?: string
  evidence?: string
  doneAt?: string
}

export interface Task {
  id: string
  revision?: number
  drops?: TaskDrop[]
  dropsPrunedThrough?: number
  workspace?: string
  workspaceId?: string
  product?: string
  assignee?: string
  updatedBy?: string
  parent?: string | null
  title: string
  /** Node kind in the mind map: "task" (default) | "decision" | "fact" | "lamp" | "state" | "blocker". */
  kind?: string
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

export function readTodos(strict = false, file = todoPath()): Task[] {
  let raw: string
  try { raw = readFileSync(file, "utf8") }
  catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") return []
    throw error
  }
  const tasks: Task[] = []
  for (const line of raw.split("\n")) {
    if (!line.trim()) continue
    try {
      const task = JSON.parse(line) as Task
      if (!task || typeof task.id !== "string" || !task.title) throw Error("Invalid task")
      tasks.push(task)
    } catch {
      if (strict) throw Error("Invalid ledger record; original snapshot was preserved")
    }
  }
  return tasks
}

type Mutation =
  | { ok: true; task: Task }
  | { ok: false; reason: string; current?: Task }

function commitSnapshot(file: string, tasks: Task[]): void {
  const body = tasks.map(t => JSON.stringify(t)).join("\n") + "\n"
  if (Buffer.byteLength(body) > MAX_TODO_BYTES) throw Error("todo ledger cap reached")
  const next = file + "." + randomUUID() + ".stage"
  try {
    writeFileSync(next, body, { encoding: "utf8", flag: "wx" })
    renameSync(next, file)
  } finally {
    rmSync(next, { force: true })
  }
}

// SQLite supplies a crash-released, cross-process mutex for new writers. The
// existing JSONL remains the source of truth and can still be read by old clients.
function withTodoWriter<T>(work: (file: string) => T, wait = 0): T {
  const file = todoPath()
  ensureDir()
  const { DatabaseSync } = require("node:sqlite") as typeof import("node:sqlite")
  const lock = new DatabaseSync(file + ".writer.sqlite")
  try {
    lock.exec("PRAGMA busy_timeout=" + wait)
    lock.exec("BEGIN IMMEDIATE")
    const result = work(file)
    // JSONL rename is the commit. Closing rolls back the otherwise empty SQLite
    // transaction and releases the mutex without a second commit operation.
    return result
  } finally {
    lock.close()
  }
}

function tryWrite(work: (todos: Task[]) => Mutation, wait = 0): Mutation {
  return withTodoWriter(file => {
    const tasks = readTodos(true,file)
    const result = work(tasks)
    if (result.ok) commitSnapshot(file,tasks)
    return result
  },wait)
}

export async function withTodoRetry<T>(operation: () => T): Promise<T> {
  const deadline = Date.now() + 1500
  for (;;) {
    try { return operation() }
    catch (error) {
      const sqliteBusy = (error as {errcode?: number})?.errcode === 5
      if (!sqliteBusy) throw error
      if (Date.now() >= deadline) throw Error("ledger_busy; retry the update")
      await new Promise(resolve => setTimeout(resolve, 8 + Math.random() * 20))
    }
  }
}

async function write(work: (todos: Task[]) => Mutation): Promise<Mutation> {
  try { return await withTodoRetry(() => tryWrite(work)) }
  catch (error) { return {ok:false,reason:(error as Error).message} }
}

export function writeTodoCompanion<T>(operation: (file: string) => T): Promise<T> {
  return withTodoRetry(() => withTodoWriter(operation))
}

function buildRevision(current: Task | undefined, next: Task): Task {
  const revision = (current?.revision ?? 0) + 1
  const updatedAt = new Date(Math.max(Date.now(),Date.parse(current?.updatedAt ?? "") || 0)).toISOString()
  const prior = current?.drops ?? []
  const drops = current && current.status !== next.status
    ? [...prior.slice(-(TASK_DROP_HISTORY-1)),{revision,from:current.status,to:next.status,at:updatedAt,
      actor:next.updatedBy,workspaceId:next.workspaceId,product:next.product}]
    : current?.drops
  const dropsPrunedThrough = current && current.status !== next.status && prior.length >= TASK_DROP_HISTORY
    ? prior[0].revision : current?.dropsPrunedThrough
  return {...next,updatedAt,revision,drops,dropsPrunedThrough}
}

function parentError(tasks: Task[], task: Task): string | undefined {
  const byId = new Map(tasks.map(item => [item.id, item]))
  const visited = new Set([task.id])
  for (let parent = task.parent; parent; parent = byId.get(parent)?.parent) {
    if (visited.has(parent)) return "mind-map parent cycle"
    if (!byId.has(parent)) return "unknown parent task"
    visited.add(parent)
  }
}

export function persistTask(task: Task): Promise<Mutation> {
  return write(tasks => {
    if (tasks.some(t => t.id === task.id)) return { ok: false, reason: "task exists" }
    const problem = parentError(tasks, task)
    if (problem) return { ok: false, reason: problem }
    const parent = tasks.find(t => t.id === task.parent)
    const stored = { ...task, workspace: task.workspace ?? parent?.workspace, product: task.product ?? parent?.product,
      workspaceId: task.workspaceId ?? parent?.workspaceId, revision: 1 }
    tasks.push(stored)
    return { ok: true, task: stored }
  })
}

export async function updateTask(id: string, expectedRevision: number | undefined,
  change: (current: Task) => Task | string): Promise<Mutation> {
  return write(tasks => {
    const index = tasks.findIndex(task => task.id === id)
    if (index < 0) return { ok: false, reason: "no task " + id }
    const current = tasks[index]
    if (expectedRevision !== undefined && expectedRevision !== (current.revision ?? 0)) {
      return { ok: false, reason: "revision_conflict; refresh the task before retrying", current }
    }
    const next = change(current)
    if (typeof next === "string") return { ok: false, reason: next, current }
    const problem = next.parent !== current.parent ? parentError(tasks, next) : undefined
    if (problem) return { ok: false, reason: problem, current }
    tasks[index] = buildRevision(current,next)
    return { ok: true, task: tasks[index] }
  })
}

// Compatibility for existing in-process callers. The MCP uses updateTask so it
// can re-read and merge changes inside the writer transaction.
export function upsertTask(updated: Task): Task | null {
  try {
    const result = tryWrite(tasks => {
      const current = tasks.findIndex(t => t.id === updated.id)
      const revision = current >= 0 ? tasks[current].revision ?? 0 : 0
      if (updated.revision !== undefined && updated.revision !== revision) {
        return { ok: false, reason: "revision_conflict" }
      }
      const task = buildRevision(current >= 0 ? tasks[current] : undefined, updated)
      if (current >= 0) tasks[current] = task
      else tasks.push(task)
      return { ok: true, task }
    }, 750)
    return result.ok ? result.task : null
  } catch { return null }
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
export function doneCheck(now = new Date().toISOString(), todos = readTodos()): DoneCheck {
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
    `scope: selected NAI-Lattice tasks`,
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
    kind: opts.kind ?? "task",
    status: opts.status ?? "open",
    createdBy: opts.createdBy ?? "agent",
    createdAt: opts.createdAt ?? now,
    workspace: opts.workspace,
    product: opts.product,
    workspaceId: opts.workspaceId,
    gated: opts.gated,
    gates: opts.gates,
  }
}
