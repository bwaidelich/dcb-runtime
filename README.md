# DCB Runtime

Runs a DCB Model, as authored in the
[DCB Playground](https://github.com/dcb-events/dcb-playground), against a
real DCB event store – [UmaDB](https://umadb.io) or
[Tephra](https://tephra.tqwewe.com). One generic WebAssembly component takes any model
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

## The HTTP API

```
hosts/rust/target/release/dcb-runtime-host serve my-model.json --store umadb://localhost:50051    # UmaDB (umadb+tls:// for TLS)
hosts/rust/target/release/dcb-runtime-host serve my-model.json --store tephra://localhost:9000   # Tephra
```

serves the model's commands at `http://127.0.0.1:8080`:

- `POST /commands/{Command}` with the arguments as a JSON object;
- `/openapi.json` – the contract, OpenAPI 3.1, generated from the model:
  arguments and published events typed by the model's own types, the
  refusals each command can give listed. Generate a client from it with
  any OpenAPI generator;
- `/docs` – Swagger UI over that contract, to try commands by hand
  (embedded; `--no-docs` turns it off);
- `/health` – whether the event store is serving.

Requests are validated against the contract before anything is decided.

| Status | Means |
|---|---|
| 200 | published: `{events, position}` |
| 422 | rejected: `{rejection, ruleIndex}` |
| 400 | the arguments do not fit the command |
| 404 | the model has no such command |
| 409 | every attempt lost the race to a concurrent append – retry |
| 500 | the command could not be decided (defective model, throwing script) |
| 503 | the event store failed |

Failures are RFC 9457 problem details. Every call to the store is
bounded (`--store-timeout`, default 10s): a store that went away is a
503, not a request that waits forever. The server keeps one instance of
the component per command decided at once (`--instances`, default the
number of CPUs, about 10 MB each). `run <model.json> <command> <args>`
decides a single command instead and prints the response body.

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
scripts/smoke.sh umadb    # a throwaway store container and the HTTP server:
scripts/smoke.sh tephra   # every status, and 40 requests racing for one seat
```

The Tephra client generates its protocol code at build time and needs
`protoc` 35.1 exactly – on the `PATH`, or named by `PROTOC`. To build
without it: `cargo build --release --no-default-features --features umadb`.

`npm test` runs every command scenario of the playground's shipped
models and fixtures through the component, against an in-memory store
(`hosts/node/memory-store.mjs`), and compares each outcome with the
playground's evaluator over the same events. It also pins the cycle:
how often the store is asked, retries, conflicts, and that an event
landing between two reads of a chained boundary is not lost. And it
validates every scenario's arguments, and every response, against the
model's generated contract.

## Layout

- `wit/runtime.wit` – the component's world.
- `src/runtime.js` – the decision cycle; `src/contract.js` – the
  model's OpenAPI document; `src/shim.js` – the in-memory storage
  `model.js` keeps its own log in.
- `scripts/build.mjs` – concatenates the sources into one module,
  `jco componentize`s it, and transpiles it for Node.
- `hosts/node/` – the Node host's in-memory store.
- `hosts/rust/` – the HTTP host (axum, wasmtime). `src/store.rs` holds
  one adapter per event store, picked by the store URL's scheme and
  each behind a Cargo feature (`umadb`, `tephra`; both by default).
  Another DCB store is another adapter under its own scheme: `head`, `read`, `append`, and
  its failed-condition error mapped to `Integrity`. Compiling the component to native code takes
  seconds, so the host keeps the result beside it
  (`dcb-runtime.wasm.cwasm`); later starts take a fraction of a second.
  `assets/swagger-ui/` is swagger-ui-dist 5.32.15, unmodified.
- `playground/` – the playground, as a pinned submodule.

## Status

- The submodule points at a local checkout of the playground
  (`feature/read-port`), which adds the read port the runtime needs.
  Switch the URL to GitHub once that branch is published.
- The component is about 14 MB: the JavaScript engine (StarlingMonkey)
  plus all of `model.js`, including the predefined models and editing
  commands the runtime never uses.

Out of scope for now: auth, a query port for projections, idempotent
appends through the event `uuid`, metrics, reloading the model, reading
an untagged projection whose handlers never read its state as
`read(query, { backwards: true, limit: 1 })`.
