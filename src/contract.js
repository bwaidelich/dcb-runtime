// ============================================================
// DCB Runtime – the loaded model's API contract, as OpenAPI 3.1.
//
// Appended by the build after the playground's sources, before
// `runtime.js`. Every type in it comes from the playground's value
// schemas (`commandArgumentsSchema`, `eventPayloadSchema`,
// `customTypeSchemas` – model.js), so the contract says what the
// evaluator accepts and publishes, in the model's own type names:
//
//   POST /commands/{Command}   arguments → 200 published | 422 rejected
//                              (one case per rejection message)
//                              | 400 | 404 | 409 | 500 | 503 (problems)
//
// The host serves this document and validates requests against it, so
// what is documented and what is enforced are one thing.
// ============================================================

// A short, stable fingerprint of the definitions – cyrb53 – so a client
// can tell that the contract it was generated from has changed.
function contractHash(text) {
  let h1 = 0xdeadbeef;
  let h2 = 0x41c6ce57;
  for (let i = 0; i < text.length; i++) {
    const c = text.charCodeAt(i);
    h1 = Math.imul(h1 ^ c, 2654435761);
    h2 = Math.imul(h2 ^ c, 1597334677);
  }
  h1 = Math.imul(h1 ^ (h1 >>> 16), 2246822507) ^ Math.imul(h2 ^ (h2 >>> 13), 3266489909);
  h2 = Math.imul(h2 ^ (h2 >>> 16), 2246822507) ^ Math.imul(h1 ^ (h1 >>> 13), 3266489909);
  return (4294967296 * (2097151 & h2) + (h1 >>> 0)).toString(16).padStart(14, '0');
}

// Component names. Custom types keep theirs – they are what a generated
// client's types should be called; events, rejections and the fixed
// shapes get a suffix, so none of them can take a custom type's name.
const eventComponent = (eventName) => `${eventName}Event`;
const rejectionComponent = (commandName) => `${commandName}Rejection`;
const PROBLEM = 'ProblemDetails';
const componentRef = (name) => ({ $ref: `#/components/schemas/${name}` });

function problemResponse(description) {
  return { description, content: { 'application/problem+json': { schema: componentRef(PROBLEM) } } };
}

function buildContract(model) {
  const commands = model['command-definitions'];
  const events = model['event-definitions'];
  const schemas = customTypeSchemas(model, componentRef);
  for (const eventName of Object.keys(events)) {
    schemas[eventComponent(eventName)] = {
      type: 'object',
      properties: {
        type: { const: eventName },
        data: eventPayloadSchema(model, eventName, componentRef),
        tags: { type: 'array', items: { type: 'string' } },
      },
      required: ['type', 'data', 'tags'],
      additionalProperties: false,
    };
  }
  schemas[PROBLEM] = {
    type: 'object',
    description: 'RFC 9457 problem details.',
    properties: {
      type: { type: 'string' },
      title: { type: 'string' },
      status: { type: 'integer' },
      detail: { type: 'string' },
      errors: { type: 'array', items: { type: 'string' }, description: 'What failed validation, one entry each.' },
    },
    required: ['title', 'status'],
  };

  const paths = {};
  for (const [commandName, body] of Object.entries(commands)) {
    // Every event the command may publish; a guard may leave any out.
    const published = [...new Set((body.publishes || []).map((e) => e && e.name).filter((n) => n && events[n]))];
    const responses = {
      200: {
        description: 'Accepted: the events published, and the position of the last.',
        content: {
          'application/json': {
            schema: {
              type: 'object',
              properties: {
                events: published.length
                  ? { type: 'array', items: { oneOf: published.map((n) => componentRef(eventComponent(n))) } }
                  : { type: 'array', maxItems: 0 },
                position: { anyOf: [{ type: 'integer', minimum: 0 }, { type: 'null' }] },
              },
              required: ['events', 'position'],
            },
          },
        },
      },
      400: problemResponse('The arguments do not fit the command.'),
      404: problemResponse('The model has no such command.'),
      409: problemResponse('Every attempt lost the race to a concurrent append; the request may be retried.'),
      500: problemResponse('The command could not be decided: the model is defective, or a script threw.'),
      503: problemResponse('The event store failed.'),
    };
    // One case per message: rules sharing a message are one outcome,
    // and which rule refused is not part of it.
    const messages = [...new Set((body.conditions || []).map((c) => c && c.rejection).filter((m) => typeof m === 'string'))];
    if (messages.length) {
      schemas[rejectionComponent(commandName)] = {
        type: 'object',
        description: `What ${commandName} can be rejected with.`,
        properties: { rejection: { enum: messages } },
        required: ['rejection'],
        additionalProperties: false,
      };
      responses[422] = {
        description: 'Rejected: one of the command\'s rules does not hold.',
        content: {
          'application/json': {
            schema: componentRef(rejectionComponent(commandName)),
            examples: Object.fromEntries(messages.map((m) => [m, { summary: m, value: { rejection: m } }])),
          },
        },
      };
    }
    paths[`/commands/${commandName}`] = {
      post: {
        operationId: commandName,
        tags: ['commands'],
        requestBody: {
          required: true,
          content: { 'application/json': { schema: commandArgumentsSchema(model, commandName, componentRef) } },
        },
        responses,
      },
    };
  }

  return {
    openapi: '3.1.0',
    info: {
      title: model.name || 'DCB Model',
      version: contractHash(JSON.stringify(definitionsToSchema(model))),
      description: 'Commands of a DCB Model, decided against a DCB event store.',
    },
    paths,
    components: { schemas },
  };
}
