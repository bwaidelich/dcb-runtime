// ============================================================
// DCB Runtime – the decider behind the WIT world (wit/runtime.wit).
//
// Appended by the build after the playground's `model.js` and
// `evaluate.js`, into one module: those are classic scripts sharing one
// scope, and everything they declare is in scope here. Nothing below
// decides anything – the playground's evaluator does, through its read
// port (`evDecideThroughPort`, evaluate.js); this is the cycle around it:
//
//   one read    read(query) → decide → pos = last position read
//               → append(events, failIfEventsMatch: query, after: pos)
//   more        head → pos → reads 1…n → decide
//               → append(events, failIfEventsMatch: all reads, after: pos)
//
// Taking `pos` from the reads when there are several would lose an
// update: an event matching read 1 that lands before one read 2
// returns would sit below `after`. Whether a command asks more than
// once is known from its boundary's chain (`deriveRounds`); should a
// second read come unannounced anyway, the decision restarts with
// `head`. A lost race – the append's condition failing – reruns the
// whole cycle, up to MAX_ATTEMPTS times.
//
// A projection is folded the same way, `foldProjection` through a port,
// with nothing to append: one read, or – a derived projection, one per
// operand – head first and every read cut off at it, so the operands
// are folded as of one position, not each as of its own.
// ============================================================

import { head, read, append } from 'dcb:runtime/store@0.1.0';

const MAX_ATTEMPTS = 3;
const RESTART_WITH_HEAD = Symbol('restart with head');

let loaded = null;
// The contract, without and with projections.
let contracts = {};

// A store that failed is no fault of the model's; the outcome says so
// (`store`), so the host can answer it as the unavailability it is.
class StoreFailure extends Error {}

// What a failed import call carries – jco hands the err payload over
// on the thrown value.
const payloadOf = (error) => (error && error.payload !== undefined ? error.payload : error);

const fromStore = (what, call) => {
  try {
    return call();
  } catch (error) {
    const payload = payloadOf(error);
    throw new StoreFailure(`${what}: ${typeof payload === 'string' ? payload : evErrorText(error)}`);
  }
};

function decide(command, args, withHead) {
  const pos = withHead ? fromStore('head', () => head()) : undefined;
  let calls = 0;
  let last;
  const port = (items) => {
    calls++;
    if (calls > 1 && !withHead) throw RESTART_WITH_HEAD;
    const answered = fromStore('read', () => read(items.map(({ types, tags }) => ({ types, tags }))));
    return answered.map(({ position, event }) => {
      if (last === undefined || position > last) last = position;
      // The position stays here: a script folds the event, and has no
      // use for a BigInt it cannot clone.
      return { type: event.eventType, data: JSON.parse(event.data) };
    });
  };
  const result = evaluateCommand(loaded, port, command, args);
  return { result, after: withHead ? pos : last };
}

function handleOnce(command, args, withHead) {
  let decided;
  try {
    decided = decide(command, args, withHead);
  } catch (error) {
    if (error === RESTART_WITH_HEAD) return handleOnce(command, args, true);
    if (error instanceof StoreFailure) return { tag: 'store', val: error.message };
    return { tag: 'evaluation', val: evErrorText(error) };
  }
  const { result, after } = decided;
  if (result.outcome === 'rejected') {
    const { rejection, index } = result.failedRule;
    // Every rule carries a message (dcb-model.schema.json `Rule`); one
    // without is a defective model.
    if (rejection === null) return { tag: 'evaluation', val: `Rule ${index} of ${command} has no rejection message.` };
    return { tag: 'rejected', val: { rejection } };
  }
  const events = result.events.map((event) => ({
    eventType: event.type,
    data: JSON.stringify(event.data),
    tags: tagsOfEvent(loaded, event.type, event.data),
  }));
  if (!events.length) return { tag: 'published', val: { events, position: undefined } };
  // A command that reads nothing has nothing to be consistent with.
  const condition = result.query.length ? { failIfEventsMatch: result.query, after } : undefined;
  try {
    return { tag: 'published', val: { events, position: append(events, condition) } };
  } catch (error) {
    const payload = payloadOf(error);
    if (payload && payload.tag === 'integrity') return null;
    return { tag: 'store', val: `append: ${payload && payload.val !== undefined ? payload.val : evErrorText(error)}` };
  }
}

// Whether a value is JSON as it stands – what a script may have built
// instead (a Set, a Map, undefined) would not survive the trip.
function isJson(value) {
  if (value === null || typeof value === 'string' || typeof value === 'boolean') return true;
  if (typeof value === 'number') return Number.isFinite(value);
  if (Array.isArray(value)) return value.every(isJson);
  if (typeof value !== 'object' || Object.getPrototypeOf(value) !== Object.prototype) return false;
  return Object.values(value).every(isJson);
}

// The projection, read by `params` – its tags and its arguments, by
// name – folded through the store.
function projectOnce(projection, params) {
  const body = loaded['projection-definitions'][projection];
  // Where every read is cut off – nothing, at an empty store's head.
  const cut = derivedOf(body) ? { at: fromStore('head', () => head()) } : null;
  let last;
  const port = (items) => {
    const answered = fromStore('read', () => read(items.map(({ types, tags }) => ({ types, tags }))));
    return answered
      .filter(({ position }) => !cut || (cut.at !== undefined && position <= cut.at))
      .map(({ position, event }) => {
        if (last === undefined || position > last) last = position;
        return { type: event.eventType, data: JSON.parse(event.data) };
      });
  };
  const tags = {};
  const args = {};
  for (const tag of projectionTagParams(body)) tags[tag.name] = evOwn(params, tag.name);
  for (const argument of body.arguments || []) args[argument.name] = evOwn(params, argument.name);
  const value = evNormalize(foldProjection(loaded, port, projection, { tags, args }));
  if (!isJson(value)) return { tag: 'evaluation', val: `Projection "${projection}" holds a value that is no JSON.` };
  return { tag: 'projected', val: { value: JSON.stringify(value), position: last } };
}

export const decider = {
  init(modelJson) {
    loaded = null;
    contracts = {};
    let envelope;
    try {
      envelope = JSON.parse(modelJson);
    } catch (error) {
      throw `The model is no JSON: ${evErrorText(error)}`;
    }
    // Read as the playground imports, but into no log; what the
    // playground would only advise against – a skipped or defective
    // definition – a runtime refuses (`executionBlockers`, model.js).
    let fromFile;
    try {
      fromFile = modelFromEnvelope(envelope);
    } catch (error) {
      throw evErrorText(error);
    }
    const { model, skipped } = fromFile;
    const blockers = executionBlockers(model, skipped);
    if (blockers.length) {
      throw blockers.map((b) => `${humanize(b.kind)} "${b.name}": ${b.message}`).join('\n');
    }
    loaded = model;
  },

  handle(command, argsJson) {
    if (!loaded) return { tag: 'evaluation', val: 'No model is loaded.' };
    const body = evOwn(loaded['command-definitions'], command);
    if (!body) return { tag: 'invalid', val: `The model has no command "${command}".` };
    let args;
    try {
      args = JSON.parse(argsJson);
    } catch (error) {
      return { tag: 'invalid', val: `The arguments are no JSON: ${evErrorText(error)}` };
    }
    if (args === null || typeof args !== 'object' || Array.isArray(args)) {
      return { tag: 'invalid', val: 'The arguments are no JSON object.' };
    }
    const chained = deriveRounds(body).length > 1;
    for (let attempt = 0; attempt < MAX_ATTEMPTS; attempt++) {
      const outcome = handleOnce(command, args, chained);
      if (outcome) return outcome;
    }
    return { tag: 'conflict' };
  },

  project(projection, paramsJson) {
    if (!loaded) return { tag: 'evaluation', val: 'No model is loaded.' };
    if (!evOwn(loaded['projection-definitions'], projection)) {
      return { tag: 'invalid', val: `The model has no projection "${projection}".` };
    }
    let params;
    try {
      params = JSON.parse(paramsJson);
    } catch (error) {
      return { tag: 'invalid', val: `The parameters are no JSON: ${evErrorText(error)}` };
    }
    if (params === null || typeof params !== 'object' || Array.isArray(params)) {
      return { tag: 'invalid', val: 'The parameters are no JSON object.' };
    }
    try {
      return projectOnce(projection, params);
    } catch (error) {
      if (error instanceof StoreFailure) return { tag: 'store', val: error.message };
      return { tag: 'evaluation', val: evErrorText(error) };
    }
  },

  contract(withProjections) {
    if (!loaded) throw 'No model is loaded.';
    const key = withProjections ? 'with' : 'without';
    if (!contracts[key]) contracts[key] = JSON.stringify(buildContract(loaded, { withProjections }));
    return contracts[key];
  },
};
