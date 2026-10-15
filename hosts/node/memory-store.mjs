// An in-memory DCB event store, implementing the component's `store`
// import for the Node host – what the conformance suite runs against.
// `onRead` lets a test append between a decision's read and its append,
// the way a concurrent writer would.

let events = [];
let hooks = {};
export const calls = { head: 0, read: 0, append: 0 };

const matches = (query, event) => query.some((item) =>
  (!item.types.length || item.types.includes(event.eventType))
  && item.tags.every((tag) => event.tags.includes(tag)));

export function reset(seed = [], { onRead } = {}) {
  events = [];
  hooks = { onRead };
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
  return events.length ? events[events.length - 1].position : undefined;
}

export function read(query) {
  calls.read++;
  const answer = events.filter(({ event }) => matches(query, event));
  if (hooks.onRead) hooks.onRead(calls.read);
  return answer;
}

export function append(newEvents, condition) {
  calls.append++;
  if (condition && events.some(({ position, event }) =>
    (condition.after === undefined || position > condition.after) && matches(condition.failIfEventsMatch, event))) {
    throw { tag: 'integrity' };
  }
  for (const event of newEvents) push(event);
  return events[events.length - 1].position;
}
