# DCB Runtime

> [!WARNING]
> **Experimental.** Interfaces and behaviour may change without notice –
> not ready for production use yet.

**Turn a DCB Model into a running command API – backed by a real event store.**

Design your model in the [DCB Playground](https://github.com/dcb-events/dcb-playground),
export it, and serve it: every command becomes an HTTP endpoint with an
OpenAPI contract, decided against PostgreSQL,
[Axon Server](https://www.axoniq.io/server),
[EventSourcingDB](https://www.eventsourcingdb.io),
[Tephra](https://tephra.tqwewe.com) or [UmaDB](https://umadb.io).

- **One binary, any model** – a generic WebAssembly component takes the
  model as data; no code generation, no glue code.
- **Same semantics as the playground** – the component _is_ the
  playground's evaluator, so the runtime cannot disagree with it.
- **Consistency built in** – DCB append conditions, automatic retries
  on concurrent writes, typed rejections.

## Not a public API

> [!CAUTION]
> This API is the core of your application – where commands are decided –
> not its edge. It knows nothing of users, sessions, authentication,
> authorization, rate limits, untrusted input or presentation: those
> belong to your application's adapters – a web UI, a public REST or
> GraphQL API, a message consumer – which call this API over a private
> network, in the sense of
> [ports and adapters](https://alistair.cockburn.us/hexagonal-architecture/).
> **Never expose it directly.**

The host says so on start when it listens beyond the machine it runs on,
and so does the contract – at the top of Swagger UI.

## Quick start

### With Docker

Nothing to install but Docker – the model fetched by URL, the events
kept in memory:

```bash
docker run --rm -p 127.0.0.1:8080:8080 ghcr.io/bwaidelich/dcb-runtime \
  serve https://raw.githubusercontent.com/dcb-events/dcb-playground/main/fixtures/course-simple.dcb.json --store memory://
```

Then try commands in Swagger UI at http://127.0.0.1:8080/docs. The events
are gone when the container stops; for a real store, see
[Docker](#docker).

### With the binary

**1. Get the host.** Download it from the [releases](../../releases) –
Linux (x86_64, aarch64) and macOS (Apple Silicon), with `SHA256SUMS` –
and unpack it. No Node or Rust needed.

> [!TIP]
> macOS refuses the unsigned binary when a browser downloaded it:
> `xattr -d com.apple.quarantine dcb-runtime-host` lets it run.

<details>
<summary><b>Or build it from source</b></summary>

Needs Node 22+ and a Rust toolchain. From the repository root:

```bash
git submodule update --init
npm install
npm run build     # build/dcb-runtime.wasm
cargo build --release --manifest-path hosts/rust/Cargo.toml
```

The binary lands in `hosts/rust/target/release/dcb-runtime-host`.

The Tephra adapter needs `protoc` 35.1 exactly – on the `PATH`, or named
by `PROTOC`. Without it, leave Tephra out:

```bash
cargo build --release --manifest-path hosts/rust/Cargo.toml \
  --no-default-features --features umadb,postgres,axonserver,eventsourcingdb
```

</details>

**2. Get a model.** Export one from the playground as a `.dcb.json` file,
or take a fixture – or pass its `https://` URL to `serve` instead of a
file:

```bash
curl -LO https://raw.githubusercontent.com/dcb-events/dcb-playground/main/fixtures/course-simple.dcb.json
```

**3. Start an event store and serve the model** – pick one from
[Event stores](#event-stores) below, e.g. PostgreSQL:

```bash
docker run --rm -d -p 5432:5432 -e POSTGRES_PASSWORD=pw postgres:17
./dcb-runtime-host serve course-simple.dcb.json \
  --store postgres://postgres:pw@localhost:5432/postgres --with-docs
```

The first start compiles the component to native code, which takes a few
seconds; later starts are fast.

**4. Call it** – try commands in Swagger UI at http://127.0.0.1:8080/docs, or:

```bash
curl -X POST http://127.0.0.1:8080/commands/DefineCourse \
  -H 'content-type: application/json' -d '{"courseId":"c1","capacity":10}'
```

<details>
<summary><b>All options</b></summary>

| Option            | Environment variable | Default                   |                                                       |
| ----------------- | -------------------- | ------------------------- | ----------------------------------------------------- |
| `--store`         | `DCB_STORE`          | `umadb://localhost:50051` | the event store, see below                            |
| `--listen`        | `DCB_LISTEN`         | `127.0.0.1:8080`          | the address to listen on                              |
| `--instances`     | `DCB_INSTANCES`      | number of CPUs            | commands decided at once, about 10 MB each            |
| `--store-timeout` | `DCB_STORE_TIMEOUT`  | `10s`                     | how long a store call may take before it is a failure |
| `--with-docs`     | `DCB_WITH_DOCS`      | off                       | serve Swagger UI at `/docs`                           |
| `--with-read`     | `DCB_WITH_READ`      | off                       | serve [`POST /read`](#reading-events)                 |
| `--wasm`          | `DCB_RUNTIME_WASM`   | built in                  | another runtime component to use                      |

The model is a file or an `http(s)://` URL, fetched once on start; for
`serve`, `DCB_MODEL` names it, too.

`dcb-runtime-host run <model.dcb.json> <command> <args>` decides a single
command instead of serving, and prints the response body.

</details>

## Docker

The image `ghcr.io/bwaidelich/dcb-runtime` – Linux, amd64 and arm64,
tagged `X.Y.Z`, `X.Y` and `latest` – holds the host and runs `serve` by
default, configured by the [environment variables](#quick-start) above.
It sets:

- `DCB_LISTEN=0.0.0.0:8080`, to be reachable from outside the container;
- `DCB_WITH_DOCS=true`, Swagger UI at `/docs`;
- `XDG_CACHE_HOME=/cache`: the first start of a container compiles the
  component, which takes a few seconds; a volume there keeps the result
  (`-v dcb-cache:/cache`).

Publish the port on `127.0.0.1` (`-p 127.0.0.1:8080:8080`), not on every
interface as `-p 8080:8080` does – see [Not a public API](#not-a-public-api).

`localhost` inside the container is the container, not your machine: a
store running on your machine is at `host.docker.internal` (Docker
Desktop; on Linux add `--add-host=host.docker.internal:host-gateway`), or
run both with Compose. `examples/docker-compose.yml` serves the
course-simple fixture over PostgreSQL:

```bash
docker compose -f examples/docker-compose.yml up
```

## Event stores

The store is picked by the scheme of the `--store` URL. Every adapter is
behind a Cargo feature of the same name, all enabled by default – except
the in-memory one, always built in.

### Without a DCB event store

Nothing but the host, or a PostgreSQL you may well run already.

#### In memory

```bash
--store memory://
```

Events kept in the host's memory, gone when it stops – to try a model out,
nothing to start first. Never for real data. Always built in.

#### PostgreSQL

No DCB store of its own: the adapter keeps the events in a table
`dcb_events`, created on connect when missing, and checks the append
condition under an advisory lock, so appends happen one at a time.

```bash
--store postgres://user:pw@host:5432/db
```

<details>
<summary><b>Example: PostgreSQL with Docker</b></summary>

```bash
docker run --rm -d -p 5432:5432 -e POSTGRES_PASSWORD=pw postgres:17
dcb-runtime-host serve course-simple.dcb.json \
  --store postgres://postgres:pw@localhost:5432/postgres --with-docs
```

</details>

- A `search_path` in the URL puts the table elsewhere:
  `?options=-csearch_path%3Dmy_schema`.
- Plain connections only for now; `sslmode=require` is refused.

### DCB event stores

Stores that speak DCB themselves, in alphabetical order.

#### Axon Server

```bash
--store axonserver://[token@]host:8124[/context]    # axonserver+tls:// for TLS
```

<details>
<summary><b>Example: Axon Server with Docker</b></summary>

```bash
docker run --rm -d -p 8124:8124 -e AXONIQ_AXONSERVER_STANDALONE_DCB=true axoniq/axonserver:latest
dcb-runtime-host serve course-simple.dcb.json \
  --store axonserver://localhost:8124 --with-docs
```

</details>

- Needs a DCB context: the standalone server's
  (`AXONIQ_AXONSERVER_STANDALONE_DCB=true`, as above), or one created
  with DCB enabled. The URL's path names it – `default` when there is
  none – and its user is the access token, when access control is on.
- A tag `key:value` becomes Axon's tag of that key and value, as Axon
  Framework writes them. The events Axon sources carry no tags, so
  `/read` returns them with `tags: []`, and it reads forwards only.
- Needs a licence beyond a 12-hour trial.
- The client is generated from the vendored `hosts/rust/proto/axonserver/`
  at build time, without `protoc`.

#### EventSourcingDB

```bash
--store eventsourcingdb://token@host:3000[/subject][?type-prefix=…]    # eventsourcingdb+tls:// for HTTPS
```

<details>
<summary><b>Example: EventSourcingDB with Docker</b></summary>

```bash
docker run --rm -d -p 3000:3000 thenativeweb/eventsourcingdb:latest run \
  --api-token secret --data-directory-temporary --http-enabled --https-enabled=false
dcb-runtime-host serve course-simple.dcb.json \
  --store eventsourcingdb://secret@localhost:3000 --with-docs
```

</details>

- EventSourcingDB knows no tags and wants event types in reverse-domain
  notation. The adapter writes each event under the type
  `<prefix>.<EventName>` (prefix `dcb.runtime`, or the URL's
  `type-prefix`), with data `{"payload": …, "tags": […]}`, to one subject
  (the URL's path, `/` when there is none).
- It reads, and checks the append condition, with EventQL over the
  events of that subject and prefix, e.g.
  `eventsourcingdb://secret@host:3000/courses?type-prefix=com.example.courses`.
- Without a licence it stores up to 25,000 events.

#### Tephra

```bash
--store tephra://host:9000
```

<details>
<summary><b>Example: Tephra with Docker</b></summary>

```bash
docker run --rm -d -p 9000:9000 git.tqwewe.com/tephra/tephra:latest
dcb-runtime-host serve course-simple.dcb.json \
  --store tephra://localhost:9000 --with-docs
```

</details>

- The client generates its protocol code at build time and needs `protoc`
  35.1 exactly (see [building from source](#quick-start)).

#### UmaDB

The default store.

```bash
--store umadb://host:50051    # umadb+tls:// for TLS
```

<details>
<summary><b>Example: UmaDB with Docker</b></summary>

```bash
docker run --rm -d -p 50051:50051 umadb/umadb:latest
dcb-runtime-host serve course-simple.dcb.json \
  --store umadb://localhost:50051 --with-docs
```

</details>

## The HTTP API

The server answers at `http://127.0.0.1:8080`:

| Endpoint                   |                                                                                                                                                                                           |
| -------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `POST /commands/{Command}` | decide a command; the arguments as a JSON object                                                                                                                                       |
| `GET /openapi.json`        | the contract, OpenAPI 3.1, generated from the model: arguments and published events typed by the model's own types, the refusals each command can give listed. Generate a client from it |
| `GET /docs`                | Swagger UI over that contract (embedded; only with `--with-docs`)                                                                                                                         |
| `POST /read`               | the events matching a DCB query, a page at a time (only with `--with-read`; [see below](#reading-events))                                                                                 |
| `GET /health`              | whether the event store is serving                                                                                                                                                        |

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
bounded (`--store-timeout`): a store that went away is a 503, not a
request that waits forever.

### Reading events

`POST /read` takes a DCB query as its body – `{"items": [{"types": […],
"tags": […]}]}`, items ORed, an item matching an event of one of its
types (any, when left out) carrying all its tags; no body, or `{}`, reads
every event – and pages with URL parameters:

- `after` – exclusive: forwards, the events after this position;
  backwards, those before it;
- `limit` – the most events a page holds, 1 to 1000, default 100;
- `backwards` – `true` for newest first.

<details>
<summary><b>Example: reading a course's events</b></summary>

```bash
curl -X POST 'http://127.0.0.1:8080/read?limit=2' \
  -H 'content-type: application/json' -d '{"items":[{"tags":["CourseId:c1"]}]}'
```

</details>

It answers `{"events": [{"position", "type", "data", "tags"}, …], "next"}`;
`next` is the position to pass as `after` for the following page, `null`
on a page short of the limit – a full last page is followed by an empty
one. 400 for an invalid query or parameter, 501 when the store cannot
read that way (Axon Server reads forwards only), 503 when it failed.
It exposes every event in the store, so it is off unless asked for.

## How it works

- **The playground decides.** The component is the playground's
  `model.js` and `evaluate.js`, unchanged, plus `src/runtime.js` – the
  cycle around a decision. There is no second implementation of the
  semantics.
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

## Development

### Build and test

Needs Node 22+.

```bash
git submodule update --init
npm install
npm run build     # build/dcb-runtime.wasm and build/node/
npm test          # conformance against the playground's own evaluator
```

`npm test` runs every command scenario of the playground's shipped
models and fixtures through the component, against an in-memory store
(`hosts/node/memory-store.mjs`), and compares each outcome with the
playground's evaluator over the same events. It also pins the cycle:
how often the store is asked, retries, conflicts, and that an event
landing between two reads of a chained boundary is not lost. And it
validates every scenario's arguments, and every response, against the
model's generated contract.

<details>
<summary><b>Smoke tests against real stores</b></summary>

Needs Docker, jq and a Rust toolchain. Each run starts a throwaway store
container and the HTTP server, and checks every status, paging through
`/read`, and 40 requests racing for one seat:

```bash
cargo build --release --manifest-path hosts/rust/Cargo.toml
scripts/smoke.sh postgres
scripts/smoke.sh axonserver
scripts/smoke.sh eventsourcingdb
scripts/smoke.sh tephra
scripts/smoke.sh umadb
scripts/smoke.sh memory
```

`scripts/smoke.sh compose` runs the same checks against
`examples/docker-compose.yml`, with the image `DCB_IMAGE` names
(the published one by default).

</details>

### Release

CI (`.github/workflows/ci.yml`) runs the conformance tests and builds the
host on every push to `main` and every pull request. A tag `vX.Y.Z` –
matching `version` in `hosts/rust/Cargo.toml` – builds the hosts and
publishes them as a GitHub release (`.github/workflows/release.yml`), and
the Linux ones as the image `ghcr.io/bwaidelich/dcb-runtime` (`Dockerfile`),
after the Compose smoke test passed against it:

`make release` bumps that version, commits, tags and pushes; without
`VERSION` it asks, suggesting the next minor version:

```bash
make release              # or: make release VERSION=0.3.0
```

### Layout

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
  its failed-condition error mapped to `Integrity`. The host embeds
  `build/dcb-runtime.wasm`, so `npm run build` comes first. Compiling the
  component to native code takes seconds, so the host keeps the result in
  the user's cache directory (`dcb-runtime/<hash>.cwasm`; beside the file,
  as `<wasm>.cwasm`, for a `--wasm` component); later starts take a
  fraction of a second.
  `assets/swagger-ui/` is swagger-ui-dist 5.32.15, unmodified.
- `Dockerfile` – the image, from the release's prebuilt Linux hosts in
  `dist/amd64/` and `dist/arm64/`; `examples/docker-compose.yml` – the
  runtime over PostgreSQL.
- `playground/` – the playground, as a pinned submodule.

## Status

- The submodule pins a commit of the playground's `main` branch.
- The component is about 14 MB: the JavaScript engine (StarlingMonkey)
  plus all of `model.js`, including the predefined models and editing
  commands the runtime never uses.

Out of scope for now: auth, a query port for projections, idempotent
appends through the event `uuid`, metrics, reloading the model, reading
an untagged projection whose handlers never read its state as
`read(query, { backwards: true, limit: 1 })`.

## License

[MIT](LICENSE)
