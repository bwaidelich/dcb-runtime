// ============================================================
// DCB Runtime – the decider behind the WIT world (wit/runtime.wit).
//
// Appended by the build after the playground's `model.js` and
// `evaluate.js`, into one module: those are classic scripts sharing one
// scope, and everything they declare is in scope here. Nothing below
// decides anything, and nothing runs the cycle around a decision
// either – the playground's `handleCommand` and `projectFromStore`
// (evaluate.js) do both, against a store port in the DCB
// specification's shapes. This is the adapter between that port and
// the WIT's `store` import: positions as BigInt on this side and as
// numbers on that, payloads as JSON text here and values there,
// `eventType` for `type`, and jco's error payloads unwrapped.
//
// One attempt per call: a lost race is `conflict`, and deciding again –
// when, how often, after how long – is the host's.
//
// Positions cross as numbers, exact up to 2^53 – far past any store
// this serves.
// ============================================================

import { head, read, append } from 'dcb:runtime/store@0.2.0';

let loaded = null;
// The contract, without and with projections.
let contracts = {};

// What a failed import call carries – jco hands the err payload over
// on the thrown value.
const payloadOf = (error) => (error && error.payload !== undefined ? error.payload : error);

const unwrapped = (call) => {
  try {
    return call();
  } catch (error) {
    const payload = payloadOf(error);
    throw new Error(typeof payload === 'string' ? payload : evErrorText(error));
  }
};

const position = (value) => (value === undefined || value === null ? undefined : Number(value));
const wirePosition = (value) => (value === undefined ? undefined : BigInt(value));

const toWire = ({ type, data, tags }) => ({ eventType: type, data: JSON.stringify(data), tags });

const store = {
  head: () => position(unwrapped(() => head())),
  read: (query) => unwrapped(() => read(query)).map((sequenced) => ({
    position: position(sequenced.position),
    event: { type: sequenced.event.eventType, data: JSON.parse(sequenced.event.data), tags: sequenced.event.tags },
  })),
  append(events, condition) {
    const wire = condition && { failIfEventsMatch: condition.failIfEventsMatch, after: wirePosition(condition.after) };
    try {
      return { position: position(append(events.map(toWire), wire)) };
    } catch (error) {
      const payload = payloadOf(error);
      if (payload && payload.tag === 'integrity') return { integrity: true };
      throw new Error(payload && payload.val !== undefined ? payload.val : evErrorText(error));
    }
  },
};

// Whether a value is JSON as it stands – what a script may have built
// instead (a Set, a Map, undefined) would not survive the trip.
function isJson(value) {
  if (value === null || typeof value === 'string' || typeof value === 'boolean') return true;
  if (typeof value === 'number') return Number.isFinite(value);
  if (Array.isArray(value)) return value.every(isJson);
  if (typeof value !== 'object' || Object.getPrototypeOf(value) !== Object.prototype) return false;
  return Object.values(value).every(isJson);
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
    const outcome = handleCommand(loaded, store, command, args);
    switch (outcome.outcome) {
      case 'published':
        return { tag: 'published', val: { events: outcome.events.map(toWire), position: wirePosition(outcome.position) } };
      case 'rejected': {
        const { rejection, index } = outcome.failedRule;
        // Every rule carries a message (dcb-model.schema.json `Rule`); one
        // without is a defective model.
        if (rejection === null) return { tag: 'evaluation', val: `Rule ${index} of ${command} has no rejection message.` };
        return { tag: 'rejected', val: { rejection } };
      }
      case 'conflict':
        return { tag: 'conflict' };
      case 'store':
        return { tag: 'store', val: outcome.message };
      default:
        return { tag: 'evaluation', val: outcome.message };
    }
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
    const outcome = projectFromStore(loaded, store, projection, params);
    if (outcome.outcome === 'store') return { tag: 'store', val: outcome.message };
    if (outcome.outcome !== 'projected') return { tag: 'evaluation', val: outcome.message };
    if (!isJson(outcome.value)) return { tag: 'evaluation', val: `Projection "${projection}" holds a value that is no JSON.` };
    return { tag: 'projected', val: { value: JSON.stringify(outcome.value), position: wirePosition(outcome.position) } };
  },

  contract(withProjections) {
    if (!loaded) throw 'No model is loaded.';
    const key = withProjections ? 'with' : 'without';
    if (!contracts[key]) contracts[key] = JSON.stringify(buildContract(loaded, { withProjections }));
    return contracts[key];
  },
};
