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
// ============================================================

import { head, read, append } from 'dcb:runtime/store@0.1.0';

const MAX_ATTEMPTS = 3;
const RESTART_WITH_HEAD = Symbol('restart with head');

let loaded = null;

// What a failed import call carries – jco hands the err payload over
// on the thrown value.
const payloadOf = (error) => (error && error.payload !== undefined ? error.payload : error);

function decide(command, args, withHead) {
  const pos = withHead ? head() : undefined;
  let calls = 0;
  let last;
  const port = (items) => {
    calls++;
    if (calls > 1 && !withHead) throw RESTART_WITH_HEAD;
    return read(items.map(({ types, tags }) => ({ types, tags }))).map(({ position, event }) => {
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
    return { tag: 'error', val: evErrorText(error) };
  }
  const { result, after } = decided;
  if (result.outcome === 'rejected') {
    const rule = result.failedRule;
    return { tag: 'rejected', val: { rejection: rule.rejection === null ? undefined : rule.rejection, ruleIndex: rule.index } };
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
    return { tag: 'error', val: `The store refused the append: ${payload && payload.val !== undefined ? payload.val : evErrorText(error)}` };
  }
}

export const decider = {
  init(modelJson) {
    let envelope;
    try {
      envelope = JSON.parse(modelJson);
    } catch (error) {
      throw `The model is no JSON: ${evErrorText(error)}`;
    }
    // The playground keeps its models in an event log of its own; here
    // that log holds the one model, and is started afresh per load.
    localStorage.clear();
    bumpLogRevision();
    let imported;
    try {
      imported = importModelFromEnvelope(envelope);
    } catch (error) {
      loaded = null;
      throw evErrorText(error);
    }
    // Import is best-effort in the playground; a runtime deciding with
    // a definition silently missing is not.
    if (imported.skipped.length) {
      loaded = null;
      throw imported.skipped.map((s) => `${s.kind} "${s.name}": ${s.reason}`).join('\n');
    }
    loaded = projectState()[imported.modelId];
  },

  handle(command, argsJson) {
    if (!loaded) return { tag: 'error', val: 'No model is loaded.' };
    let args;
    try {
      args = JSON.parse(argsJson);
    } catch (error) {
      return { tag: 'error', val: `The arguments are no JSON: ${evErrorText(error)}` };
    }
    const body = evOwn(loaded['command-definitions'], command);
    const chained = !!body && deriveRounds(body).length > 1;
    for (let attempt = 0; attempt < MAX_ATTEMPTS; attempt++) {
      const outcome = handleOnce(command, args, chained);
      if (outcome) return outcome;
    }
    return { tag: 'conflict' };
  },
};
