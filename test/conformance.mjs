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
      return { rejected: { rejection: result.failedRule.rejection, ruleIndex: result.failedRule.index } };
    }
    return { published: result.events.map((e) => ({ type: e.type, data: e.data })) };
  } catch (error) {
    return { error: String(error.message || error) };
  }
}

function componentOutcome(outcome) {
  switch (outcome.tag) {
    case 'rejected': return { rejected: { rejection: outcome.val.rejection ?? null, ruleIndex: outcome.val.ruleIndex } };
    case 'published': return { published: outcome.val.events.map((e) => ({ type: e.eventType, data: JSON.parse(e.data) })) };
    case 'error': return { error: outcome.val };
    default: return { [outcome.tag]: true };
  }
}

// ---------------------------------------------------------------
// Every command scenario decides as the playground decides.
// ---------------------------------------------------------------
for (const { name, envelope } of envelopes()) {
  check(`${name}: every command scenario decides as the playground does`, () => {
    const model = oracleModel(envelope);
    decider.init(JSON.stringify(envelope));
    for (const scenario of Object.values(model['scenario-definitions'] || {})) {
      const log = sandbox.scenarioLog(scenario);
      const args = (scenario.when && scenario.when.arguments) || {};
      store.reset(log.map((e) => storedEvent(model, e.type, e.data)));
      const actual = componentOutcome(decider.handle(scenario.command, JSON.stringify(args)));
      eq(canonical(actual), canonical(oracleOutcome(model, log, scenario.command, args)), scenario.name);
      if (actual.published && actual.published.length) {
        eq(store.all().length, log.length + actual.published.length, `${scenario.name} – appended`);
      }
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

  check('an unloadable model and unknown commands are errors', () => {
    let refused = null;
    try { decider.init('{}'); } catch (error) { refused = error.payload ?? error.message; }
    eq(typeof refused, 'string', 'init refused');
    eq(handle('DefineCourse', {}).tag, 'error', 'nothing loaded');
    decider.init(JSON.stringify(envelope));
    eq(handle('Nope', {}).tag, 'error', 'no such command');
    eq(decider.handle('DefineCourse', '{').tag, 'error', 'no JSON');
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
