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
//
//   GET /projections/{Projection}   (with projections) its tags and
//                              arguments as query parameters – a
//                              composite one as JSON – → 200 the value
//
// What the model decides is said here; what goes wrong around it – a
// request that does not fit, a lost race, a failed store – is the
// host's to answer, so the host adds those responses, and the problem
// details they carry. It serves the document and validates requests
// against it, so what is documented and what is enforced are one thing.
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
const componentRef = (name) => ({ $ref: `#/components/schemas/${name}` });

// A projection's tags and arguments, each a query parameter: a scalar
// by its schema, a composite – a record of fields – as JSON.
function projectionQueryParameters(model, body) {
  const parameter = (name, typeName) => {
    const schema = valueTypeSchema(model, typeName, componentRef);
    return classifyType(model, typeName).composite
      ? { name, in: 'query', required: true, content: { 'application/json': { schema } } }
      : { name, in: 'query', required: true, schema };
  };
  return [
    ...projectionTagParams(body).map((tag) => parameter(tag.name, tag.tagType)),
    ...(body.arguments || []).filter((a) => a && a.name).map((a) => parameter(a.name, a.propertyType)),
  ];
}

function projectionOperation(model, projectionName, body) {
  const value = propertySchema(model, { propertyType: body.valueType, isList: body.isList }, componentRef);
  return {
    get: {
      operationId: `get${projectionName}`,
      tags: ['projections'],
      parameters: projectionQueryParameters(model, body),
      responses: {
        200: {
          description: 'The value, folded from the events at the time of the request; null where nothing is known yet.',
          content: {
            'application/json': {
              schema: {
                type: 'object',
                properties: {
                  value: { anyOf: [value, { type: 'null' }] },
                  position: {
                    anyOf: [{ type: 'integer', minimum: 1 }, { type: 'null' }],
                    description: 'The position of the last event folded; null when it folded none.',
                  },
                },
                required: ['value', 'position'],
              },
            },
          },
        },
      },
    },
  };
}

function buildContract(model, { withProjections = false } = {}) {
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

  if (withProjections) {
    for (const [projectionName, body] of Object.entries(model['projection-definitions'])) {
      paths[`/projections/${projectionName}`] = projectionOperation(model, projectionName, body);
    }
  }

  return {
    openapi: '3.1.0',
    info: {
      title: model.name || 'DCB Model',
      version: contractHash(JSON.stringify(definitionsToSchema(model))),
      description: [
        'Commands of a DCB Model, decided against a DCB event store.',
        '',
        '**⚠️ Not a public API.** This API is the core of your application – where commands are decided –'
        + ' not its edge. It knows nothing of users, sessions, authentication, authorization, rate limits,'
        + ' untrusted input or presentation: those belong to your application\'s adapters – a web UI, a public'
        + ' REST or GraphQL API, a message consumer – which call this API over a private network.'
        + ' Never expose it directly.',
      ].join('\n'),
    },
    paths,
    components: { schemas },
  };
}
