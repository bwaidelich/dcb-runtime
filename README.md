# DCB Runtime

Runs a DCB Model, as authored in the
[DCB Playground](https://github.com/dcb-events/dcb-playground), against a
real DCB event store. One generic WebAssembly component takes any model
as data (its JSON envelope) and decides commands; the host connects it to
a store.

Experimental. The reasoning behind it is in the playground's
`docs/research/2026-10-15-dcb-runtime-wasm.md`.

## How it works

- **The playground decides.** The component is the playground's
  `model.js` and `evaluate.js`, unchanged, plus `src/runtime.js` – the
  cycle around a decision. There is no second implementation of the
  semantics, so the runtime cannot disagree with the playground.
- **Ports** (`wit/runtime.wit`):
  - exports `decider`: `init(model-json)`, `handle(command, args-json)`
    → `published | rejected | conflict | error`;
  - imports `store`: `head`, `read(query)`, `append(events, condition)`,
    as the DCB specification describes them. The host implements it.
- **The decision cycle.** The evaluator asks the store once per round
  of reads, items merged by tag set:
  - one read: `read` → decide → `append(…, after: last position read)`;
  - a chained boundary: `head` → reads → decide →
    `append(…, failIfEventsMatch: every read, after: head)`.

  A failed append condition reruns the whole cycle, up to three times,
  then reports `conflict`.
- **Events in the store**: `type` is the event name, `data` the payload
  as JSON, `tags` the tags the model derives for it.

## Build and test

Needs Node 22+.

```
git submodule update --init
npm install
npm run build     # build/dcb-runtime.wasm and build/node/
npm test          # conformance against the playground's own evaluator
```

Against a real store – needs Docker and a Rust toolchain:

```
cargo build --release --manifest-path hosts/rust/Cargo.toml
scripts/smoke-umadb.sh   # a throwaway UmaDB container, driven through the Rust host
```

`npm test` runs every command scenario of the playground's shipped
models and fixtures through the component, against an in-memory store
(`hosts/node/memory-store.mjs`), and compares each outcome with the
playground's evaluator over the same events. It also pins the cycle:
how often the store is asked, retries, conflicts, and that an event
landing between two reads of a chained boundary is not lost.

## Layout

- `wit/runtime.wit` – the component's world.
- `src/runtime.js` – the decision cycle; `src/shim.js` – the in-memory
  storage `model.js` keeps its own log in.
- `scripts/build.mjs` – concatenates the sources into one module,
  `jco componentize`s it, and transpiles it for Node.
- `hosts/node/` – the Node host's in-memory store.
- `hosts/rust/` – a host over UmaDB (wasmtime, `umadb-client`):
  `dcb-runtime-host <model.json> <command> <args-json>` prints the
  outcome as JSON. `UMADB_URL` names the store (default
  `http://localhost:50051`), `DCB_RUNTIME_WASM` the component (default
  `build/dcb-runtime.wasm`). Compiling the component to native code
  takes seconds, so the host keeps the result beside it
  (`dcb-runtime.wasm.cwasm`); later runs take a fraction of a second.
- `playground/` – the playground, as a pinned submodule.

## Status

- The submodule points at a local checkout of the playground
  (`feature/read-port`), which adds the read port the runtime needs.
  Switch the URL to GitHub once that branch is published.
- The component is about 14 MB: the JavaScript engine (StarlingMonkey)
  plus all of `model.js`, including the predefined models and editing
  commands the runtime never uses.

Next: a long-running host – the Rust one decides a single command per
process – exposing commands over HTTP or a queue.

Out of scope for now: a query port for projections, idempotent appends
through the event `uuid`, reading an untagged projection whose handlers
never read its state as `read(query, { backwards: true, limit: 1 })`.
