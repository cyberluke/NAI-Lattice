import { test } from "node:test"
import assert from "node:assert/strict"
import { mkdtempSync, writeFileSync, rmSync } from "node:fs"
import { tmpdir } from "node:os"
import { join } from "node:path"
// Import the compiled todo module (dist) so the test exercises what ships.
import { readTodos, upsertTask, newTask, doneCheck } from "../dist/todo.js"

/** Run a scenario with an isolated TODO_PATH (temp ledger). */
function withTodo(t, fn) {
  const dir = mkdtempSync(join(tmpdir(), "todo-"))
  const path = join(dir, "todo.jsonl")
  const old = process.env.TODO_PATH
  process.env.TODO_PATH = path
  try { return fn(path) } finally { process.env.TODO_PATH = old; try { rmSync(dir, { recursive: true, force: true }) } catch {} }
}

test("todo gate: empty list is a done milestone (all_items)", () => {
  withTodo(null, () => {
    const c = doneCheck()
    assert.equal(c.done, true)
    assert.equal(c.milestone, "all_items")
  })
})

test("todo gate: open gated item blocks done until build gate recorded", () => {
  withTodo(null, (path) => {
    writeFileSync(path, JSON.stringify(newTask("fetrum", { gated: { needsBuild: true }, id: "a" })) + "\n")
    let c = doneCheck()
    assert.equal(c.done, false)
    assert.equal(c.reason, "open_remaining")
    // record build gate ok -> now done via build_done milestone
    const t = readTodos()[0]
    upsertTask({ ...t, gates: { build: { kind: "build", ok: true, cmd: "cargo build", doneAt: new Date().toISOString() } } })
    c = doneCheck()
    assert.equal(c.done, true)
    assert.equal(c.milestone, "build_done")
  })
})

test("todo gate: open UNgated item always blocks done (can't be gated away)", () => {
  withTodo(null, (path) => {
    writeFileSync(path, JSON.stringify(newTask("plain", { id: "b" })) + "\n")
    const c = doneCheck()
    assert.equal(c.done, false)
    assert.equal(c.reason, "open_remaining")
  })
})

test("todo gate: blocked task blocks done regardless of gates", () => {
  withTodo(null, (path) => {
    writeFileSync(
      path,
      JSON.stringify(newTask("b1", { status: "blocked", id: "c" })) +
        "\n" +
        JSON.stringify(newTask("gated", { gated: { needsBuild: true }, gates: { build: { kind: "build", ok: true, cmd: "x", doneAt: "t" } }, id: "d" })) +
        "\n",
    )
    const c = doneCheck()
    assert.equal(c.done, false)
    assert.equal(c.reason, "blocked")
  })
})

test("todo gate: commitChecklist carries timestamps + evidence", () => {
  withTodo(null, (path) => {
    const doneTask = { ...newTask("done1", { id: "e" }), status: "done", doneAt: "2026-01-01T00:00:00Z" }
    writeFileSync(path, JSON.stringify(doneTask) + "\n")
    const c = doneCheck()
    assert.equal(c.done, true)
    assert.ok(c.commitChecklist.some((l) => l.includes("finished 2026-01-01T00:00:00Z")))
    assert.ok(c.commitChecklist.some((l) => l.includes("completed-at")))
  })
})