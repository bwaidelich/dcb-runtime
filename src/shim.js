// The browser storage `model.js` keeps its own event log in – in
// memory, since the runtime's model lives only as long as the instance.
const memoryStorage = () => {
  const map = new Map();
  return {
    getItem: (key) => (map.has(key) ? map.get(key) : null),
    setItem: (key, value) => { map.set(key, String(value)); },
    removeItem: (key) => { map.delete(key); },
    clear: () => { map.clear(); },
  };
};
globalThis.localStorage = memoryStorage();
globalThis.sessionStorage = memoryStorage();
