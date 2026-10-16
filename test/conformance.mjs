// ============================================================
// DCB Runtime – conformance.
//
// The component must decide what the playground decides. Every command
// scenario of the shipped models and the playground's fixtures runs
// twice: through the component, against the in-memory store seeded with
// the scenario's Given, and through the playground's own evaluator over
// the same Given as a log – the oracle. Then the cycle around the
// decision: how often the store is asked, and what a concurrent append
// does to it.
//
// Run `npm run build` first; then `npm test`.
// ============================================================
import { createRequire } from 'node:module';
import { readFileSync, readdirSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const require = createRequire(import.meta.url);
const { createSandbox, loadApp, makeChecker } = require(join(root, 'playground', 'app', 'test-harness.js'));
const { decider } = await import(join(root, 'build', 'node', 'dcb-runtime.js'));
const { default: Ajv2020 } = await import('ajv/dist/2020.js');
const store = await import(join(root, 'hosts', 'node', 'memory-store.mjs'));

const { sandbox, store: oracleStorage } = createSandbox();
loadApp(sandbox, ['model.js', 'evaluate.js'], { trailer: 'globalThis.PREDEFINED_MODELS = PREDEFINED_MODELS;' });
const { check, eq, finish } = makeChecker();

// The oracle's model, read back from the very envelope the component
// loads – so both decide over the same definitions.
function oracleModel(envelope) {
  oracleStorage.clear();
  sandbox.bumpLogRevision();
  const { modelId } = sandbox.importModelFromEnvelope(envelope);
  return sandbox.projectState()[modelId];
}

function envelopes() {
  const out = sandbox.PREDEFINED_MODELS.map((shipped, index) => {
    oracleStorage.clear();
    sandbox.bumpLogRevision();
    const id = sandbox.loadPredefinedModel(index);
    return { name: shipped.slug, envelope: sandbox.buildShareEnvelope(sandbox.projectState()[id], []) };
  });
  const fixtures = join(root, 'playground', 'fixtures');
  for (const file of readdirSync(fixtures)) {
    out.push({ name: file, envelope: JSON.parse(readFileSync(join(fixtures, file), 'utf8')) });
  }
  return out;
}

const storedEvent = (model, type, data) => ({
  eventType: type, data: JSON.stringify(data), tags: sandbox.tagsOfEvent(model, type, data),
});

const canonical = (value) => JSON.parse(JSON.stringify(value, (key, v) => {
  if (typeof v === 'bigint') return Number(v);
  if (v && typeof v === 'object' && !Array.isArray(v)) {
    return Object.fromEntries(Object.keys(v).sort().map((k) => [k, v[k]]));
  }
  return v;
}));

// Both sides as one comparable shape.
function oracleOutcome(model, log, command, args) {
  try {
    const result = sandbox.evaluateCommand(model, log, command, args);
    if (result.outcome === 'rejected') {
      return { rejected: result.failedRule.rejection };
    }
    return { published: result.events.map((e) => ({ type: e.type, data: e.data })) };
  } catch (error) {
    return { error: String(error.message || error) };
  }
}

function componentOutcome(outcome) {
  switch (outcome.tag) {
    case 'rejected': return { rejected: outcome.val.rejection };
    case 'published': return { published: outcome.val.events.map((e) => ({ type: e.eventType, data: JSON.parse(e.data) })) };
    case 'invalid': case 'evaluation': case 'store': return { error: outcome.val };
    default: return { [outcome.tag]: true };
  }
}

// The loaded model's contract as a validator: a command's request body,
// or its `published` / `rejected` response, checked against the schema
// the contract states for it.
function contractValidator() {
  const contract = JSON.parse(decider.contract(false));
  const ajv = new Ajv2020({ strict: false, allErrors: true });
  ajv.addSchema({ ...contract, $id: 'contract' });
  const pointer = (command, part) => {
    const at = `contract#/paths/~1commands~1${command}/post`;
    if (part === 'request') return `${at}/requestBody/content/application~1json/schema`;
    return `${at}/responses/${part === 'published' ? 200 : 422}/content/application~1json/schema`;
  };
  return (command, part, value) => {
    const validate = ajv.compile({ $ref: pointer(command, part) });
    return validate(value) ? [] : validate.errors.map((e) => `${e.instancePath} ${e.message}`);
  };
}

// The contract with projections: a projection's parameter – by its
// schema, or the one its JSON content has – or its `200` body, checked
// against what the contract states for it.
function projectionValidator() {
  const contract = JSON.parse(decider.contract(true));
  const ajv = new Ajv2020({ strict: false, allErrors: true });
  ajv.addSchema({ ...contract, $id: 'contract' });
  const at = (projection) => `contract#/paths/~1projections~1${projection}/get`;
  const check = (pointer, value) => {
    const validate = ajv.compile({ $ref: pointer });
    return validate(value) ? [] : validate.errors.map((e) => `${e.instancePath} ${e.message}`);
  };
  return {
    parameter(projection, name, value) {
      const parameters = contract.paths[`/projections/${projection}`].get.parameters;
      const index = parameters.findIndex((p) => p.name === name);
      if (index < 0) return [`no parameter ${name}`];
      const schema = parameters[index].content ? 'content/application~1json/schema' : 'schema';
      return check(`${at(projection)}/parameters/${index}/${schema}`, value);
    },
    response(projection, body) {
      return check(`${at(projection)}/responses/200/content/application~1json/schema`, body);
    },
  };
}

// A projection's parameters as one flat object: its tags, then its
// arguments.
const projectionParams = (read) => ({ ...read.tags, ...read.args });

const project = (projection, params) => decider.project(projection, JSON.stringify(params));

// What the host sends for a projected value.
function projectedBody(outcome) {
  return {
    value: JSON.parse(outcome.val.value),
    position: outcome.val.position === undefined ? null : Number(outcome.val.position),
  };
}

// A response body as the host sends it.
function responseBody(outcome) {
  if (outcome.tag === 'rejected') return { rejection: outcome.val.rejection };
  return {
    events: outcome.val.events.map((e) => ({ type: e.eventType, data: JSON.parse(e.data), tags: e.tags })),
    position: outcome.val.position === undefined ? null : Number(outcome.val.position),
  };
}

// ---------------------------------------------------------------
// Every command scenario decides as the playground decides.
// ---------------------------------------------------------------
for (const { name, envelope } of envelopes()) {
  check(`${name}: every command scenario decides as the playground does`, () => {
    const model = oracleModel(envelope);
    decider.init(JSON.stringify(envelope));
    const fits = contractValidator();
    for (const scenario of Object.values(model['scenario-definitions'] || {})) {
      const log = sandbox.scenarioLog(scenario);
      const args = (scenario.when && scenario.when.arguments) || {};
      store.reset(log.map((e) => storedEvent(model, e.type, e.data)));
      eq(fits(scenario.command, 'request', args), [], `${scenario.name} – arguments fit the contract`);
      const outcome = decider.handle(scenario.command, JSON.stringify(args));
      if (outcome.tag === 'published' || outcome.tag === 'rejected') {
        eq(fits(scenario.command, outcome.tag, responseBody(outcome)), [], `${scenario.name} – ${outcome.tag} fits the contract`);
      }
      const actual = componentOutcome(outcome);
      eq(canonical(actual), canonical(oracleOutcome(model, log, scenario.command, args)), scenario.name);
      if (actual.published && actual.published.length) {
        eq(store.all().length, log.length + actual.published.length, `${scenario.name} – appended`);
      }
    }
  });
}

// ---------------------------------------------------------------
// Every projection scenario folds as the playground folds.
// ---------------------------------------------------------------
for (const { name, envelope } of envelopes()) {
  check(`${name}: every projection scenario folds as the playground does`, () => {
    const model = oracleModel(envelope);
    decider.init(JSON.stringify(envelope));
    const fits = projectionValidator();
    for (const spec of Object.values(model['projection-scenario-definitions'] || {})) {
      const log = sandbox.scenarioLog(spec);
      const tags = {};
      for (const [tag, literal] of Object.entries(spec.tags || {})) tags[tag] = literal && literal.tagValue;
      const params = projectionParams({ tags, args: spec.arguments || {} });
      store.reset(log.map((e) => storedEvent(model, e.type, e.data)));
      for (const [param, value] of Object.entries(params)) {
        eq(fits.parameter(spec.projection, param, value), [], `${spec.name} – ${param} fits the contract`);
      }
      const outcome = project(spec.projection, params);
      let expected;
      try {
        expected = { value: sandbox.evNormalize(sandbox.deriveProjectionScenarioThen(model, spec)) };
      } catch (error) {
        expected = { error: String(error.message || error) };
      }
      if (outcome.tag !== 'projected') {
        eq({ [outcome.tag]: outcome.val }, expected, spec.name);
        continue;
      }
      const body = projectedBody(outcome);
      eq(fits.response(spec.projection, body), [], `${spec.name} – the value fits the contract`);
      eq(canonical({ value: body.value }), canonical(expected), spec.name);
      eq(store.calls.append, 0, `${spec.name} – nothing appended`);
    }
  });
}

// ---------------------------------------------------------------
// Reading a projection.
// ---------------------------------------------------------------
{
  const envelope = envelopes().find((e) => e.name === 'course-simple').envelope;
  const model = oracleModel(envelope);

  check('a projection is one read, no head, at the last position it folds', () => {
    decider.init(JSON.stringify(envelope));
    store.reset([
      storedEvent(model, 'CourseDefined', { courseId: 'c1', capacity: 10 }),
      storedEvent(model, 'CourseDefined', { courseId: 'c2', capacity: 5 }),
      storedEvent(model, 'CourseCapacityChanged', { courseId: 'c1', newCapacity: 12 }),
      storedEvent(model, 'CourseDefined', { courseId: 'c3', capacity: 5 }),
    ]);
    const outcome = project('CourseCapacity', { courseId: 'c1' });
    eq(outcome.tag, 'projected', 'projected');
    eq(projectedBody(outcome), { value: 12, position: 3 }, 'value and position');
    eq({ ...store.calls }, { head: 0, read: 1, append: 0 }, 'calls');
  });

  check('a projection that folds nothing is at no position', () => {
    decider.init(JSON.stringify(envelope));
    store.reset([storedEvent(model, 'CourseDefined', { courseId: 'c2', capacity: 5 })]);
    eq(projectedBody(project('CourseCapacity', { courseId: 'c1' })), { value: 0, position: null }, 'initial value');
  });

  check('unknown projections, broken parameters and a failing store say which they are', () => {
    decider.init(JSON.stringify(envelope));
    store.reset([]);
    eq(project('Nope', {}).tag, 'invalid', 'no such projection');
    eq(decider.project('CourseCapacity', '{').tag, 'invalid', 'no JSON');
    eq(decider.project('CourseCapacity', '[1]').tag, 'invalid', 'no object');
    eq(project('CourseCapacity', {}).tag, 'evaluation', 'a tag missing – the contract catches it first');
    store.reset([], { failing: 'read' });
    eq(project('CourseCapacity', { courseId: 'c1' }).tag, 'store', 'store');
  });

  check('the contract names projections only when asked to', () => {
    decider.init(JSON.stringify(envelope));
    const without = JSON.parse(decider.contract(false));
    eq(Object.keys(without.paths).filter((p) => p.startsWith('/projections/')), [], 'none without');
    const contract = JSON.parse(decider.contract(true));
    eq(Object.keys(contract.paths).filter((p) => p.startsWith('/projections/')).sort(),
      Object.keys(model['projection-definitions']).map((p) => `/projections/${p}`).sort(), 'one path per projection');
    const capacity = contract.paths['/projections/CourseCapacity'].get;
    eq(capacity.parameters, [{ name: 'courseId', in: 'query', required: true, schema: { $ref: '#/components/schemas/CourseId' } }],
      'its tags as parameters, by type name');
    eq(Object.keys(contract.paths).filter((p) => p.startsWith('/commands/')).length,
      Object.keys(without.paths).length, 'the commands either way');
  });
}

{
  // Composite tags and arguments travel as JSON; a tag and an argument
  // of one name would share a parameter.
  const base = envelopes().find((e) => e.name === 'course-simple').envelope;
  const envelope = JSON.parse(JSON.stringify(base));
  envelope.customTypeDefinitions.push({
    name: 'CourseRef',
    properties: [{ name: 'courseId', propertyType: 'CourseId' }],
  });
  envelope.projectionDefinitions.push({
    name: 'CapacityAbove',
    tags: [{ name: 'course', tagType: 'CourseRef' }],
    arguments: [{ name: 'threshold', propertyType: 'integer' }, { name: 'ref', propertyType: 'CourseRef' }],
    valueType: 'boolean',
    isList: false,
    script: { initialState: false },
    handlers: [{ event: 'CourseDefined', code: 'event.data.capacity > args.threshold' }],
  });

  check('a composite tag or argument is a JSON parameter', () => {
    decider.init(JSON.stringify(envelope));
    const parameters = JSON.parse(decider.contract(true)).paths['/projections/CapacityAbove'].get.parameters;
    eq(parameters.map((p) => [p.name, p.content ? 'json' : 'plain']),
      [['course', 'json'], ['threshold', 'plain'], ['ref', 'json']], 'composites as JSON');
    eq(parameters[0].content['application/json'].schema, { $ref: '#/components/schemas/CourseRef' }, 'by type name');
  });

  check('a projection whose tag and argument share a name does not load', () => {
    const clashing = JSON.parse(JSON.stringify(base));
    const capacity = clashing.projectionDefinitions.find((p) => p.name === 'CourseCapacity');
    capacity.arguments = [{ name: 'courseId', propertyType: 'integer' }];
    let refused = null;
    try { decider.init(JSON.stringify(clashing)); } catch (error) { refused = error.payload ?? error.message; }
    eq(typeof refused === 'string' && refused.includes('courseId'), true, 'refused, naming it');
  });
}

{
  const envelope = envelopes().find((e) => e.name === 'content-decisions-derived.dcb.json').envelope;
  const model = oracleModel(envelope);

  check('a derived projection takes head and folds nothing after it', () => {
    decider.init(JSON.stringify(envelope));
    // While the current text is read, the document is published: the
    // published text, read next, would be newer than the current one.
    store.reset([
      storedEvent(model, 'DocumentAdded', { id: 'd1' }),
      storedEvent(model, 'TextUpdated', { docId: 'd1', text: 'a' }),
    ], {
      onRead: (n) => { if (n === 1) store.push(storedEvent(model, 'DocumentPublished', { docId: 'd1', text: 'a' })); },
    });
    const outcome = project('DocumentHasPendingChanges', { documentId: 'd1' });
    eq(outcome.tag, 'projected', 'projected');
    eq(projectedBody(outcome), { value: true, position: 2 }, 'as of head');
    eq(store.calls.head, 1, 'head taken');
  });
}

{
  const base = envelopes().find((e) => e.name === 'content-decisions-scripted.dcb.json').envelope;
  const model = oracleModel(base);
  const scripted = (code) => {
    const envelope = JSON.parse(JSON.stringify(base));
    const status = envelope.projectionDefinitions.find((p) => p.name === 'DocumentStatus');
    delete status.script.exposes;
    status.handlers.find((h) => h.event === 'DocumentAdded').code = code;
    return envelope;
  };

  check('a script that throws, or holds what JSON cannot, is an evaluation failure', () => {
    for (const [what, code] of [['throws', 'null.x'], ['a Set', 'new Set([1])'], ['undefined', 'undefined']]) {
      decider.init(JSON.stringify(scripted(code)));
      store.reset([storedEvent(model, 'DocumentAdded', { id: 'd1' })]);
      eq(project('DocumentStatus', { documentId: 'd1' }).tag, 'evaluation', what);
    }
  });
}

// ---------------------------------------------------------------
// The cycle around a decision.
// ---------------------------------------------------------------
const byName = (slug) => envelopes().find((e) => e.name === slug).envelope;
const handle = (command, args) => decider.handle(command, JSON.stringify(args));

{
  const envelope = byName('course-simple');
  const model = oracleModel(envelope);
  const seed = () => [
    storedEvent(model, 'CourseDefined', { courseId: 'c1', capacity: 10 }),
    storedEvent(model, 'StudentRegistered', { studentId: 's1' }),
  ];

  check('reads that hang off the arguments are one read, no head', () => {
    decider.init(JSON.stringify(envelope));
    store.reset(seed());
    eq(handle('SubscribeStudentToCourse', { courseId: 'c1', studentId: 's1' }).tag, 'published', 'published');
    eq({ ...store.calls }, { head: 0, read: 1, append: 1 }, 'calls');
  });

  check('an append that lost the race is decided again', () => {
    decider.init(JSON.stringify(envelope));
    // Someone else defines c1 while DefineCourse decides.
    store.reset([], {
      onRead: (n) => { if (n === 1) store.push(storedEvent(model, 'CourseDefined', { courseId: 'c1', capacity: 5 })); },
    });
    const outcome = handle('DefineCourse', { courseId: 'c1', capacity: 10 });
    eq(outcome.tag, 'rejected', 'the second attempt sees the course');
    eq(outcome.val.rejection, 'Course already exists', 'refused');
    eq({ ...store.calls }, { head: 0, read: 2, append: 1 }, 'calls');
  });

  check('losing every race is a conflict', () => {
    decider.init(JSON.stringify(envelope));
    let n = 0;
    store.reset(seed(), {
      onRead: () => { store.push(storedEvent(model, 'StudentRegistered', { studentId: 's1' })); n++; },
    });
    eq(handle('SubscribeStudentToCourse', { courseId: 'c1', studentId: 's1' }).tag, 'conflict', 'conflict');
    eq(store.calls.append, 3, 'three attempts');
  });

  check('a command that publishes nothing is not appended', () => {
    decider.init(JSON.stringify(envelope));
    store.reset(seed());
    eq(handle('DefineCourse', { courseId: 'c1', capacity: 10 }).tag, 'rejected', 'rejected');
    eq(store.calls.append, 0, 'no append');
  });

  check('an unloadable model, unknown commands and broken arguments say which they are', () => {
    let refused = null;
    try { decider.init('{}'); } catch (error) { refused = error.payload ?? error.message; }
    eq(typeof refused, 'string', 'init refused');
    eq(handle('DefineCourse', {}).tag, 'evaluation', 'nothing loaded');
    decider.init(JSON.stringify(envelope));
    eq(handle('Nope', {}).tag, 'invalid', 'no such command');
    eq(decider.handle('DefineCourse', '{').tag, 'invalid', 'no JSON');
    eq(decider.handle('DefineCourse', '[1]').tag, 'invalid', 'no object');
    eq(handle('DefineCourse', {}).tag, 'evaluation', 'a required argument missing – the contract catches it first');
  });

  check('a failing store is a store outcome, at every call', () => {
    decider.init(JSON.stringify(envelope));
    for (const failing of ['read', 'append']) {
      store.reset([], { failing });
      eq(handle('DefineCourse', { courseId: 'c1', capacity: 1 }).tag, 'store', failing);
    }
  });

  check('the contract names the model\'s types and refusals', () => {
    decider.init(JSON.stringify(envelope));
    const contract = JSON.parse(decider.contract(false));
    eq([contract.openapi, contract.info.title], ['3.1.0', model.name], 'header');
    eq(Object.keys(contract.paths).sort(), Object.keys(model['command-definitions']).map((c) => `/commands/${c}`).sort(),
      'one path per command');
    const define = contract.paths['/commands/DefineCourse'].post;
    eq(define.requestBody.content['application/json'].schema.properties.courseId, { $ref: '#/components/schemas/CourseId' },
      'arguments by type name');
    eq(define.responses[422].content['application/json'].schema, { $ref: '#/components/schemas/DefineCourseRejection' },
      'refusals by command name');
    eq(contract.components.schemas.DefineCourseRejection.properties.rejection.enum, ['Course already exists'],
      'the refusals it can give');
    eq(Object.keys(define.responses[422].content['application/json'].examples), ['Course already exists'],
      'one example per refusal');
    const fits = contractValidator();
    eq(fits('DefineCourse', 'request', { courseId: 'c1', capacity: 2, extra: 1 }).length > 0, true, 'closed');
    eq(fits('DefineCourse', 'request', { courseId: 'c1', capacity: '2' }).length > 0, true, 'typed');
    eq(fits('DefineCourse', 'rejected', { rejection: 'Course already exists', ruleIndex: 0 }).length > 0, true,
      'which rule refused is not part of it');
  });
}

{
  const envelope = byName('course-schedules');
  const model = oracleModel(envelope);
  const seed = () => [
    storedEvent(model, 'CourseDefined', { courseId: 'c1', capacity: 9, slots: ['2026-03-01T09'] }),
    storedEvent(model, 'CourseDefined', { courseId: 'c2', capacity: 9, slots: ['2026-03-01T11'] }),
    storedEvent(model, 'StudentRegistered', { studentId: 's1' }),
    storedEvent(model, 'StudentSubscribedToCourse', { courseId: 'c1', studentId: 's1' }),
    storedEvent(model, 'StudentSubscribedToCourse', { courseId: 'c2', studentId: 's1' }),
  ];

  check('a chained boundary takes head first and reads once per link', () => {
    decider.init(JSON.stringify(envelope));
    store.reset(seed());
    const outcome = handle('RescheduleCourse', { courseId: 'c1', slots: ['2026-03-01T13'] });
    eq(outcome.tag, 'published', 'published');
    eq({ ...store.calls }, { head: 1, read: 3, append: 1 }, 'calls');
  });

  check('an event matching the first read is not lost below a later read\'s answer', () => {
    decider.init(JSON.stringify(envelope));
    // While c1 is read, s2 subscribes to c1 – which the first read
    // covers – and then c2 is rescheduled, which the third read returns.
    // An `after` taken from the reads would sit above the subscription.
    store.reset(seed(), {
      onRead: (n) => {
        if (n !== 1) return;
        store.push(storedEvent(model, 'StudentSubscribedToCourse', { courseId: 'c1', studentId: 's2' }));
        store.push(storedEvent(model, 'CourseRescheduled', { courseId: 'c2', slots: ['2026-03-01T11'] }));
      },
    });
    eq(handle('RescheduleCourse', { courseId: 'c1', slots: ['2026-03-01T13'] }).tag, 'published', 'published');
    eq(store.calls.append, 2, 'the first append was refused and the decision rerun');
  });
}

finish();
