// Generates the ops_v2 encoding fixtures from the real canvas page code (Canvas App's
// `src/ops.ts`), so `tests/ops_v2_fixtures.rs` can check the server writes the same Y.Doc.
//
//   CANVAS_DIR=path/to/canvas-app TZ=UTC bun crates/engine/tests/fixtures/ops_v2/gen.ts
//
// CANVAS_DIR needs its `node_modules` (yjs). Each scenario writes `<name>.json`: the ops
// it ran (in steps), what each step answered, a typed dump of the `shapes` and
// `comments` roots (Y.Map / Y.Text / Y.Array kept apart from plain values, numbers tagged
// with the lib0 encoding they get: `i` varint, `f32`, `f64`) and the page's `readCanvas`.

import { writeFileSync } from 'node:fs'
import { dirname, join } from 'node:path'

const CANVAS = process.env.CANVAS_DIR
if (!CANVAS) throw new Error('set CANVAS_DIR to a Canvas App checkout with node_modules')
const OUT = dirname(new URL(import.meta.url).pathname)

// "Now" is fixed: the page reads both `Date.now()` and `new Date()` (today's date).
const T0 = 1_790_000_000_000
const RealDate = Date
class FixedDate extends RealDate {
  constructor(...args: unknown[]) {
    if (args.length === 0) super(T0)
    else super(...(args as [number]))
  }
  static now() {
    return T0
  }
}
;(globalThis as { Date: DateConstructor }).Date = FixedDate as unknown as DateConstructor

const Y = await import(Bun.resolveSync('yjs', join(CANVAS, 'src')))
const { CanvasStore } = await import(join(CANVAS, 'src/canvas/doc.ts'))
const { applyOps, readCanvas } = await import(join(CANVAS, 'src/ops.ts'))

const BY = 'Planner'
const BY_ID = 'agent:planner'
const PEOPLE = [
  { id: 'u-grace', name: 'Grace Hopper', email: 'grace@example.com' },
  { id: 'u-ada', name: 'Ada Lovelace', email: 'ada@example.com' },
]

function dump(v: unknown): unknown {
  if (v instanceof Y.Map) {
    const o: Record<string, unknown> = {}
    for (const k of [...v.keys()].sort()) o[k] = dump(v.get(k))
    return { 'Y.Map': o }
  }
  if (v instanceof Y.Text) return { 'Y.Text': v.toString() }
  if (v instanceof Y.Array) return { 'Y.Array': v.toArray().map(dump) }
  if (typeof v === 'number') {
    if (Number.isInteger(v) && Math.abs(v) <= 0x7fffffff) return { i: v }
    return Math.fround(v) === v ? { f32: v } : { f64: v }
  }
  if (v === undefined) return { undefined: true }
  if (Array.isArray(v)) return v.map(dump)
  if (v && typeof v === 'object') {
    const o: Record<string, unknown> = {}
    for (const k of Object.keys(v).sort()) o[k] = dump((v as Record<string, unknown>)[k])
    return o
  }
  return v
}

interface Scenario {
  name: string
  /** Shapes written as they are, before any op (e.g. an older client's checklist). */
  seed?: Record<string, Record<string, unknown>>
  steps: unknown[][]
}

function run(s: Scenario) {
  const doc = new Y.Doc()
  const store = new CanvasStore(doc)
  if (s.seed) {
    doc.transact(() => {
      for (const [id, props] of Object.entries(s.seed!)) {
        const m = new Y.Map()
        store.shapes.set(id, m)
        for (const [k, v] of Object.entries(props)) m.set(k, v)
      }
    })
  }
  const ctx = { store, viewportCenter: () => ({ x: 0, y: 0 }), people: () => PEOPLE }
  const steps = s.steps.map(ops => {
    const r = applyOps(ctx, { as: { id: BY_ID, name: BY }, ops })
    return {
      ops,
      applied: r.applied,
      ids: r.ids,
      errors: r.errors.map((e: { index: number; error: string }) => ({ index: e.index, error: e.error })),
    }
  })
  const read = readCanvas({ store, canvas: { id: 'c', name: 'c', kind: 'shared' }, viewport: null, selection: [], now: T0 })
  const out = {
    name: s.name,
    now: T0,
    by: BY,
    byId: BY_ID,
    people: PEOPLE,
    seed: s.seed ?? {},
    steps,
    doc: { shapes: dump(doc.getMap('shapes')), comments: dump(doc.getMap('comments')) },
    read: { shapes: read.shapes, comments: read.comments },
  }
  writeFileSync(join(OUT, `${s.name}.json`), JSON.stringify(out, null, 1) + '\n')
  console.log(`${s.name}: ${steps.map(x => `${x.applied}/${x.ops.length}`).join(' ')}`)
}

const legacy = (rows: [string, string][]) => ({
  id: 'old',
  type: 'checklist',
  x: 0,
  y: 600,
  w: 324,
  h: 49 + 28 + 34 * rows.length + 14,
  color: 'green',
  z: 1,
  title: 'Old',
  columns: ['Yes', 'No'],
  rows: rows.map(([id, label]) => ({ id, label })),
  createdAt: T0 - 1000,
  updatedAt: T0 - 1000,
})

const SCENARIOS: Scenario[] = [
  {
    name: 'checklist_card',
    steps: [
      [
        { op: 'add', shape: { id: 'c', type: 'checklist', x: 0, y: 0, title: 'Lunch · Fri', rows: ['Ann', 'Bob', { label: 'Cy', id: 'cy' }], picks: { Ann: 'Yes', cy: 'no' }, notes: { Bob: 'late\nby 10 min' }, assignees: { Ann: ['grace'], Cy: ['Someone Else', { email: 'ada@example.com' }] } } },
        { op: 'add', shape: { id: 'todo', type: 'checklist', x: 400, y: 0, columns: ['Done'], rows: ['Book a table', 'Order cake'], picks: { 'book a table': true } } },
        { op: 'add', shape: { id: 'n', type: 'sticky', view: 'checklist', x: 800, y: 0, text: '### Chores\n- [ ] Dishes\n- [x] Bins' } },
        { op: 'add', shape: { id: 'plain', type: 'sticky', x: 1200, y: 0, text: '### Packing\n- [ ] Tent\n- [x] Stove\nnot a task' } },
        { op: 'add', shape: { type: 'checklist', rows: ['A'], picks: { A: 'Maybe' } } },
        { op: 'add', shape: { type: 'checklist', rows: ['A'], notes: { Z: 'x' } } },
      ],
      [
        { op: 'update', id: 'c', patch: { picks: { Ann: null, Bob: 'Yes', Cy: true }, notes: { Ann: 'veggie', Bob: '' }, assignees: { Bob: ['ada'], Ann: null } } },
        { op: 'update', id: 'todo', patch: { title: 'Before Friday', picks: { 'Order cake': 'done' } } },
        { op: 'update', id: 'n', patch: { picks: { Dishes: true }, notes: { Bins: 'Tuesday' } } },
        { op: 'update', id: 'plain', patch: { view: 'checklist' } },
        { op: 'update', id: 'c', patch: { title: 'nope', picks: { Zed: 'Yes' } } },
      ],
      [
        { op: 'update', id: 'c', patch: { rows: ['Cy', 'Dee', 'Ann'], columns: ['Yes', 'No', 'Maybe'], picks: { Dee: 'maybe' } } },
        { op: 'update', id: 'todo', patch: { rows: ['Order cake', 'Balloons'] } },
        { op: 'update', id: 'n', patch: { view: null } },
        { op: 'resize', id: 'c', w: 50, h: 50 },
        { op: 'move', id: 'todo', dx: 10, dy: -5 },
      ],
      [
        { op: 'update', id: 'c', patch: { columns: ['Yes', 'Maybe'] } },
        { op: 'update', id: 'plain', patch: { text: '### Packing v2\n- [ ] Tent\n- [ ] Stove' } },
        { op: 'update', id: 'todo', patch: { color: 'blue', w: 500 } },
      ],
    ],
  },
  {
    name: 'checklist_legacy',
    seed: { old: legacy([['ann', 'Ann'], ['bob', 'Bob'], ['cy', 'Cy']]) },
    steps: [
      [{ op: 'update', id: 'old', patch: { picks: { ann: 'Yes', Bob: 'no' }, notes: { cy: 'maybe later' }, assignees: { Ann: ['grace', 'Ada Lovelace'] } } }],
      [
        { op: 'update', id: 'old', patch: { rows: ['Bob', { label: 'Dee', id: 'dee' }, 'Ann'], columns: ['Yes', 'No', 'Maybe'], picks: { Dee: 'maybe' }, title: 'Older' } },
        { op: 'update', id: 'old', patch: { picks: { Zed: 'Yes' } } },
      ],
      [
        { op: 'update', id: 'old', patch: { columns: ['Yes', 'Maybe'], notes: { Bob: 'x' } } },
        { op: 'resize', id: 'old', w: 10, h: 10 },
      ],
    ],
  },
  {
    name: 'timeline',
    steps: [
      [
        { op: 'add', shape: { id: 't', type: 'timeline', x: 0, y: 0, title: 'Q4 launch', start: '2026-10-01', end: '2026-12-31', items: [
          { date: '2026-10-14', label: 'Kickoff' },
          { date: '2026-10-20', end: '2026-11-03', label: 'Build', color: 'blue' },
          { date: '2026-11-10', label: 'Beta', text: 'invite **50** users\nthen widen', color: 'pink' },
          { date: '2026-12-01', label: 'GA', kind: 'bubble' },
        ] } },
        { op: 'add', shape: { id: 'bare', type: 'timeline', x: 0, y: 600 } },
        { op: 'add', shape: { id: 'span', type: 'timeline', x: 1000, y: 0, title: 'Span', items: [{ date: '2026-03-02', label: 'a' }, { date: '2027-08-09', label: 'b' }] } },
        { op: 'add', shape: { id: 'txt', type: 'sticky', view: 'timeline', x: 1000, y: 600, text: '### Trip\n2026-09-01 → 2026-09-30 · by week\n- 2026-09-05 · Fly out\n- 2026-09-20 · Fly back' } },
        { op: 'add', shape: { id: 'nt', type: 'sticky', x: 2000, y: 0, text: '- 2026-09-25 · Party\n- 2026-10-02 · Cleanup' } },
        { op: 'add', shape: { type: 'timeline', editItems: { a: null } } },
        { op: 'add', shape: { type: 'timeline', items: [{ date: 'soon' }] } },
      ],
      [
        { op: 'update', id: 't', patch: { addItems: [{ date: '2026-10-28', label: 'Review', color: 'green' }], editItems: { '2026-10-14 · Kickoff': { label: 'Kick-off', text: 'all hands' }, Build: { end: '2026-11-07' }, '3': { color: null }, GA: null } } },
        { op: 'update', id: 'bare', patch: { title: 'Renamed', start: '2026-09-01', end: '2026-10-15', step: 'day' } },
        { op: 'update', id: 'nt', patch: { view: 'timeline' } },
        { op: 'update', id: 'txt', patch: { editItems: { 'Nope': null } } },
        { op: 'update', id: 'txt', patch: { items: [{ date: '2026-09-07', label: 'Only' }], step: 'month' } },
      ],
      [
        { op: 'resize', id: 't', w: 200, h: 10 },
        { op: 'update', id: 'span', patch: { view: null } },
        { op: 'update', id: 'bare', patch: { text: '### Fresh\n- 2026-09-30 · One' } },
      ],
    ],
  },
  {
    name: 'web',
    steps: [
      [
        { op: 'add', shape: { id: 'w', type: 'web', x: 0, y: 0, url: 'https://example.com/docs' } },
        { op: 'add', shape: { id: 'l', type: 'link', x: 1000, y: 0, url: 'https://example.org', live: true, title: 'Org' } },
        { op: 'add', shape: { id: 'card', type: 'link', x: 1000, y: 800, url: 'https://example.net/a' } },
        { op: 'add', shape: { type: 'web', url: 'mailto:a@example.com' } },
        { op: 'add', shape: { type: 'link', url: 'https://example.com', live: 'yes' } },
      ],
      [
        { op: 'update', id: 'w', patch: { live: false } },
        { op: 'update', id: 'card', patch: { live: true } },
        { op: 'resize', id: 'l', w: 100, h: 100 },
        { op: 'update', id: 'l', patch: { url: 'https://example.com/other' } },
      ],
    ],
  },
  {
    name: 'comments',
    steps: [
      [
        { op: 'add', shape: { id: 's', type: 'sticky', x: 0, y: 0, text: 'Ship it?\nsecond line' } },
        { op: 'add', shape: { id: 'f', type: 'frame', x: 400, y: 0, title: 'Slide 1' } },
        { op: 'comment', on: 's', text: '  Looks good\r\nto me  ', id: 'th1' },
        { op: 'comment', on: 'shape:f', text: 'Title?', id: 'th2' },
        { op: 'comment', on: 'nope', text: 'x' },
        { op: 'comment', on: 's', text: '   ' },
        { op: 'comment', on: 's', text: 'dup', id: 'th1' },
      ],
      [
        { op: 'reply', thread: 'th1', text: 'Agreed' },
        { op: 'resolve', thread: 'comment:th1' },
        { op: 'resolve', thread: 'th2', resolved: true },
        { op: 'reply', thread: 'missing', text: 'x' },
      ],
      [
        { op: 'reply', thread: 'th2', text: 'Reopening' },
        { op: 'resolve', thread: 'th1', resolved: false },
        { op: 'delete', id: 'f' },
      ],
    ],
  },
]

for (const s of SCENARIOS) run(s)
