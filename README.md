# DCB Runtime

Runs a DCB Model, as authored in the
[DCB Playground](https://github.com/dcb-events/dcb-playground), against a
real DCB event store – [UmaDB](https://umadb.io),
[Tephra](https://tephra.tqwewe.com), [Axon Server](https://www.axoniq.io/server),
[EventSourcingDB](https://www.eventsourcingdb.io) or PostgreSQL. One generic WebAssembly component takes any model
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

## Usage

Needs Node 22+, a Rust toolchain, and Docker (or any running DCB store).

1. **Fetch the playground and build the component** – from the repository root:

   ```
   git submodule update --init
   npm install
   npm run build     # build/dcb-runtime.wasm
   ```

2. **Build the host:**

   ```
   cargo build --release --manifest-path hosts/rust/Cargo.toml
   ```

   The Tephra adapter needs `protoc` 35.1; without it, add
   `--no-default-features --features umadb,postgres,axonserver,eventsourcingdb`.

3. **Export the model** from the playground as JSON, e.g. `my-model.json`,
   or use a fixture such as `playground/fixtures/course-simple.json`.

4. **Start an event store**, e.g.:

   ```
   docker run --rm -d -p 50051:50051 umadb/umadb:latest                       # UmaDB
   docker run --rm -d -p 9000:9000 git.tqwewe.com/tephra/tephra:latest        # Tephra
   docker run --rm -d -p 5432:5432 -e POSTGRES_PASSWORD=pw postgres:17        # PostgreSQL
   docker run --rm -d -p 8124:8124 -e AXONIQ_AXONSERVER_STANDALONE_DCB=true axoniq/axonserver:latest   # Axon Server
   docker run --rm -d -p 3000:3000 thenativeweb/eventsourcingdb:latest run \
     --api-token secret --data-directory-temporary --http-enabled --https-enabled=false   # EventSourcingDB
   ```

5. **Serve the model** – from the repository root, since the host loads
   `build/dcb-runtime.wasm` relative to it (`--wasm` or `DCB_RUNTIME_WASM`
   to point elsewhere):

   ```
   hosts/rust/target/release/dcb-runtime-host serve my-model.json --store umadb://localhost:50051
   ```

   The first start compiles the component to native code, which takes a few
   seconds; later starts are fast. Options (`--listen`, `--instances`,
   `--store-timeout`, `--no-docs`) are listed by `--help`, and each can also be
   set through an environment variable (`DCB_STORE`, `DCB_LISTEN`, …).

6. **Call it** – try commands in Swagger UI at http://127.0.0.1:8080/docs, or:

   ```
   curl -X POST http://127.0.0.1:8080/commands/DefineCourse \
     -H 'content-type: application/json' -d '{"courseId":"c1","capacity":10}'
   ```

## The HTTP API

```
hosts/rust/target/release/dcb-runtime-host serve my-model.json --store umadb://localhost:50051    # UmaDB (umadb+tls:// for TLS)
hosts/rust/target/release/dcb-runtime-host serve my-model.json --store tephra://localhost:9000   # Tephra
hosts/rust/target/release/dcb-runtime-host serve my-model.json --store postgres://user:pw@localhost:5432/db   # PostgreSQL
hosts/rust/target/release/dcb-runtime-host serve my-model.json --store axonserver://localhost:8124   # Axon Server (axonserver+tls:// for TLS)
hosts/rust/target/release/dcb-runtime-host serve my-model.json --store eventsourcingdb://secret@localhost:3000   # EventSourcingDB (eventsourcingdb+tls:// for HTTPS)
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

| Status | Means                                                               |
| ------ | ------------------------------------------------------------------- |
| 200    | published: `{events, position}`                                     |
| 422    | rejected: `{rejection}`, one of the command's rejection messages    |
| 400    | the arguments do not fit the command                                |
| 404    | the model has no such command                                       |
| 409    | every attempt lost the race to a concurrent append – retry          |
| 500    | the command could not be decided (defective model, throwing script) |
| 503    | the event store failed                                              |

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
scripts/smoke.sh postgres
scripts/smoke.sh axonserver
scripts/smoke.sh eventsourcingdb
```

The Tephra client generates its protocol code at build time and needs
`protoc` 35.1 exactly – on the `PATH`, or named by `PROTOC`. To build
without it: `cargo build --release --no-default-features --features umadb,postgres,axonserver,eventsourcingdb`.

PostgreSQL is no DCB store of its own: the adapter keeps the events in a
table `dcb_events`, created on connect when missing (a `search_path` in
the URL, `?options=-csearch_path%3Dmy_schema`, puts it elsewhere), and
checks the append condition under an advisory lock, so appends happen one
at a time. Plain connections only for now; `sslmode=require` is refused.

Axon Server needs a DCB context: the standalone server's
(`AXONIQ_AXONSERVER_STANDALONE_DCB=true`, as above), or one created with
DCB enabled. The URL's path names it – `default` when there is none – and
its user is the access token, when access control is on:
`axonserver://token@host:8124/my-context`. A tag `key:value` becomes Axon's
tag of that key and value, as Axon Framework writes them. Axon Server
needs a licence beyond a 12-hour trial. Its client is generated from the
vendored `hosts/rust/proto/axonserver/` at build time, without `protoc`.

EventSourcingDB knows no tags and wants event types in reverse-domain
notation. The adapter writes each event under the type
`<prefix>.<EventName>` (prefix `dcb.runtime`, or the URL's `type-prefix`),
with data `{"payload": …, "tags": […]}`, to one subject (the URL's path,
`/` when there is none). It reads, and checks the append condition, with
EventQL over the events of that subject and prefix:
`eventsourcingdb://secret@host:3000/courses?type-prefix=com.example.courses`.
Without a licence it stores up to 25,000 events.

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
  each behind a Cargo feature (`umadb`, `tephra`, `postgres`, `axonserver`,
  `eventsourcingdb`; all by default).
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
