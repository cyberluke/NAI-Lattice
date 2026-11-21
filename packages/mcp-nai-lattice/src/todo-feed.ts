import { createHash, randomUUID } from "node:crypto"
import { readFileSync, renameSync, rmSync, watch, writeFileSync } from "node:fs"
import { basename, dirname } from "node:path"
import { readTodos, todoPath, writeTodoCompanion, type Task, type TaskStatus } from "./todo.js"

type Follow = { id: string; after: number; on: TaskStatus[] }
type Acknowledgment = { id: string; revision: number }
const MAX_FOLLOWS = 32
const MAX_FEED_PAGE = 12
const fanPattern = /^[A-Za-z0-9._:-]{1,140}$/

export function followPath(fan: string, ledger = todoPath()): string {
  if (!fanPattern.test(fan)) throw Error("Use a valid fan ID. Satin shows it for each chat.")
  return ledger + ".fans-" + createHash("sha256").update(fan).digest("hex") + ".json"
}

export function readFollowing(fan: string, ledger = todoPath()): Follow[] {
  try {
    const data = JSON.parse(readFileSync(followPath(fan,ledger),"utf8"))
    if (!Array.isArray(data) || data.length > MAX_FOLLOWS) throw Error("Invalid feed snapshot")
    return data
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") return []
    throw error
  }
}

function writeFollowing(fan: string, ledger: string, following: Follow[]) {
  const file = followPath(fan,ledger)
  const stage = file + "." + randomUUID() + ".stage"
  try {
    writeFileSync(stage, JSON.stringify(following), {flag:"wx"})
    renameSync(stage,file)
  } finally {
    rmSync(stage,{force:true})
  }
}

export function followTask(fan: string,id: string,on?: TaskStatus[]) {
  return writeTodoCompanion(ledger => {
    const following = readFollowing(fan,ledger)
    const entry = following.find(item => item.id === id)
    if (!on) {
      if (entry) writeFollowing(fan,ledger,following.filter(item => item.id !== id))
      return {following:following.filter(item => item.id !== id)}
    }
    const task = readTodos(false,ledger).find(item => item.id === id)
    if (!task) throw Error("No matching task to follow")
    if (!entry && following.length >= MAX_FOLLOWS) throw Error("Feed is full. Unfollow an arc to make room.")
    // Repeated follow calls preserve the read cursor.
    const next = {id, after:entry?.after ?? task.revision ?? 0, on:[...new Set(on)]}
    const updated = following.filter(item => item.id !== id).concat(next)
    writeFollowing(fan,ledger,updated)
    return {following:updated}
  })
}

export function acknowledgeFeed(fan: string,acks: Acknowledgment[]) {
  return writeTodoCompanion(ledger => {
    const following = readFollowing(fan,ledger)
    const tasks = new Map(readTodos(false,ledger).map(task => [task.id,task]))
    for (const ack of acks) {
      const cursor = following.find(item => item.id === ack.id)
      if (!cursor || ack.revision <= cursor.after) continue
      // Never advance into the future or across an event that was not visible
      // under the subscription filter.
      const drop = tasks.get(ack.id)?.drops?.find(drop => drop.revision === ack.revision && cursor.on.includes(drop.to))
      if (!drop) throw Error("Cannot acknowledge an unknown drop. Refresh the feed.")
      cursor.after = drop.revision
    }
    writeFollowing(fan,ledger,following)
  })
}

export function makeFeed(following: Follow[], tasks: Task[]) {
  const byId = new Map(tasks.map(task => [task.id,task]))
  const gaps: {id:string; prunedThrough:number}[] = []
  const events = following.flatMap(({id,after,on}) => {
    const task = byId.get(id)
    if (!task) return []
    if ((task.dropsPrunedThrough ?? 0) > after) {
      gaps.push({id,prunedThrough:task.dropsPrunedThrough!})
    }
    return (task.drops ?? []).filter(drop => drop.revision > after && on.includes(drop.to))
      .map(drop => ({id,title:task.title.slice(0,180),workspaceId:task.workspaceId,product:task.product,...drop}))
  }).sort((a,b) => b.at.localeCompare(a.at) || a.id.localeCompare(b.id) || b.revision-a.revision)
  const latest: Record<string,number> = Object.create(null)
  for (const drop of events) latest[drop.id] = Math.max(latest[drop.id] ?? 0,drop.revision)
  return {events:events.slice(0,MAX_FEED_PAGE),total:events.length,more:events.length > MAX_FEED_PAGE,gaps,latest}
}

export function readFeed(fan: string, ledger = todoPath()) {
  const following = readFollowing(fan,ledger)
  return makeFeed(following,following.length ? readTodos(false,ledger) : [])
}

export const readFeedTasks = (ledger: string) => readTodos(false,ledger)

// Watch the containing directory because task commits replace the inode.
// Ignore SQLite activity and staged writes. A bounded debounce collapses bursts
// into one snapshot read, while revisions in the ledger preserve each drop.
export function watchTodoChanges(ledger: string, refresh: (file: string | null) => void, failed?: () => void) {
  const name = basename(ledger)
  let timer: ReturnType<typeof setTimeout> | undefined
  const pending = new Set<string | null>()
  const watcher = watch(dirname(ledger),{persistent:false},(_event,file) => {
    if (file && file !== name && !(file.startsWith(name + ".fans-") && file.endsWith(".json"))) return
    pending.add(file)
    if (timer) return
    timer = setTimeout(() => {
      timer = undefined
      const paths = [...pending]
      pending.clear()
      refresh(paths.length === 1 ? paths[0] : null)
    },65)
    timer.unref?.()
  })
  watcher.on("error",() => {
    clearTimeout(timer)
    watcher.close()
    failed?.()
  })
  return () => {
    clearTimeout(timer)
    watcher.close()
  }
}
