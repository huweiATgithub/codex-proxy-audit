import { readFileSync, writeFileSync } from 'node:fs';
import { z } from 'zod';
import { endpoints, errorResponse, MAX_RESPONSE_BYTES } from '../contracts/audit.js';

const jsonSchema = (schema) => {
  const { $schema, ...value } = z.toJSONSchema(schema, { target: 'draft-2020-12', io: 'input' });
  return value;
};
const paths = Object.fromEntries(Object.entries(endpoints).map(([operationId, endpoint]) => {
  const query = jsonSchema(endpoint.query);
  return [`/${endpoint.path}`, { get: {
    operationId, summary: endpoint.summary,
    parameters: Object.entries(query.properties).map(([name, schema]) => ({
      name, in: 'query', required: query.required?.includes(name) || false, schema,
    })),
    responses: {
      200: { description: `UTF-8 JSON; at most ${MAX_RESPONSE_BYTES} bytes.`, content: { 'application/json': { schema: jsonSchema(endpoint.response) } } },
      ...Object.fromEntries([400, 404, 410, 500, 503].map((status) => [status, { description: 'Plugin error; see docs/design.md section 5.3.', content: { 'application/json': { schema: jsonSchema(errorResponse) } } }])),
      401: { description: 'Host administrator session expired; body is host-defined.' },
      403: { description: 'Host administrator permission denied; body is host-defined.' },
    },
  } }];
}));
const document = {
  openapi: '3.1.0', info: { title: 'Prompt Audit management contract', version: '1.0.0', description: 'Paths are relative to the plugin instance. Transport is window.codexProxyPlugin.request; no direct HTTP base URL. See docs/design.md section 5.3 for cursor and content sequencing invariants.' },
  paths,
};
const output = JSON.stringify(document, null, 2) + '\n';
const path = new URL('../contracts/openapi.json', import.meta.url);
if (process.argv.includes('--check')) {
  if (readFileSync(path, 'utf8') !== output) throw new Error('Contract export is stale. Run npm run contract:export.');
} else writeFileSync(path, output);
