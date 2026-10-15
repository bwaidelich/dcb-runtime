// An in-memory DCB event store, implementing the component's `store`
// import for the Node host – what the conformance suite runs against.
// `onRead` lets a test append between a decision's read and its append,
// the way a concurrent writer would.

let events = [];
let hooks = {};
let failing = null;
export const calls = { head: 0, read: 0, append: 0 };

const matches = (query, event) => query.some((item) =>
  (!item.types.length || item.types.includes(event.eventType))
  && item.tags.every((tag) => event.tags.includes(tag)));

// `failing` names a call the store refuses, as an unreachable store would.
export function reset(seed = [], { onRead, failing: fail = null } = {}) {
  events = [];
  hooks = { onRead };
  failing = fail;
  calls.head = calls.read = calls.append = 0;
  for (const event of seed) push(event);
}

export function push(event) {
  events.push({ position: BigInt(events.length + 1), event });
}

export function all() {
  return events;
}

export function head() {
  calls.head++;
  if (failing === 'head') throw 'unavailable';
  return events.length ? events[events.length - 1].position : undefined;
}

export function read(query) {
  calls.read++;
  if (failing === 'read') throw 'unavailable';
  const answer = events.filter(({ event }) => matches(query, event));
  if (hooks.onRead) hooks.onRead(calls.read);
  return answer;
}

export function append(newEvents, condition) {
  calls.append++;
  if (failing === 'append') throw { tag: 'other', val: 'unavailable' };
  if (condition && events.some(({ position, event }) =>
    (condition.after === undefined || position > condition.after) && matches(condition.failIfEventsMatch, event))) {
    throw { tag: 'integrity' };
  }
  for (const event of newEvents) push(event);
  return events[events.length - 1].position;
}
