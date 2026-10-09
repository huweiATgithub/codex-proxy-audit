import test from 'node:test';
import assert from 'node:assert/strict';
import { createRedactor, parseSessionRows, contextWindow, buildSessionFixtures } from '../scripts/generate-session-fixtures.js';
import { createDemoService } from '../demo/service.js';
import { readFile } from 'node:fs/promises';

const row = (type, payload, timestamp = '2026-10-09T10:00:00Z') => ({ type, payload, timestamp });
const meta = (extra = {}) => row('session_meta', { id: '11111111-2222-3333-4444-555555555555', timestamp: '2026-10-09T09:00:00Z', cwd: '/home/alice/projects/acme-widget', source: 'cli', ...extra });
const message = (role, text, extra = {}) => row('response_item', { type: 'message', role, content: [{ type: role === 'user' ? 'input_text' : 'output_text', text }], ...extra });
const input = (text, turnId = 'aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee') => message('user', text, { internal_chat_message_metadata_passthrough: { turn_id: turnId, content_item_kinds: ['user.text'] } });

test('session parser imports response_item dialogue once and preserves Agent phases', () => {
  const session = parseSessionRows([
    meta(), row('turn_context', { model: 'example-model', effort: 'high', turn_id: 'aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee' }),
    message('user', 'injected instructions', { internal_chat_message_metadata_passthrough: { content_item_kinds: ['agents_md.instructions'] } }),
    message('developer', 'internal policy'), input('Please investigate.'),
    message('assistant', 'I will inspect the code.', { phase: 'commentary', id: 'assistant-1' }),
    message('assistant', 'I will inspect the code.', { phase: 'commentary', id: 'assistant-1' }),
    row('event_msg', { type: 'agent_message', message: 'I will inspect the code.' }),
    row('response_item', { type: 'reasoning', content: [{ type: 'text', text: 'private reasoning' }] }),
    row('response_item', { type: 'function_call_output', output: 'tool output' }),
    message('assistant', 'The plan has two steps.', { phase: 'final_answer' }), input('Please continue.'),
  ]);
  assert.equal(session.messages.length, 4);
  assert.deepEqual(session.messages.map(({ role, phase }) => [role, phase]), [['user', null], ['assistant', 'commentary'], ['assistant', 'final_answer'], ['user', null]]);
  assert.deepEqual(session.messages[0].requestMetadata, { model: 'example-model', reasoningEffort: 'high' });
  const fixture = buildSessionFixtures([session]);
  assert.equal(fixture.audits.length, 1);
  assert.deepEqual(fixture.audits[0].context.map((entry) => entry.role), ['user', 'assistant', 'assistant']);
  assert.equal(fixture.audits[0].repositories, null);
  assert.equal(fixture.stats.agentMessages, 2);
  assert.doesNotMatch(JSON.stringify(fixture), /private reasoning|tool output|internal policy|injected instructions|alice|acme-widget|11111111/);
});

test('classification remains block-local and absent/mismatched classifications remain readable', () => {
  const mixed = message('user', 'unused');
  mixed.payload.content = [{ type: 'input_text', text: 'setup' }, { type: 'input_text', text: 'actual' }, { type: 'input_image', image_url: 'data:image/png;base64,private' }];
  mixed.payload.internal_chat_message_metadata_passthrough = { content_item_kinds: ['environments.environment_context', 'user.answered_question', 'user.image'] };
  const unknown = message('user', '# AGENTS.md quoted by a human');
  const mismatched = message('user', 'unknown classification');
  mismatched.payload.internal_chat_message_metadata_passthrough = { content_item_kinds: ['agents_md.instructions', 'user.text'] };
  const session = parseSessionRows([meta(), mixed, unknown, mismatched]);
  assert.equal(session.messages.length, 3);
  assert.deepEqual(session.messages[0].blocks.map((block) => block.inputKind), ['user.answered_question', 'user.image']);
  assert.equal(session.messages[0].blocks[1].kind, 'nontext');
  assert.equal(session.messages[1].blocks[0].text, '# AGENTS.md quoted by a human');
  assert.equal(session.messages[1].blocks[0].inputKind, null);
  assert.equal(session.messages[2].blocks[0].inputKind, null);
  assert.doesNotMatch(JSON.stringify(session), /base64/);
});

test('same text and same turn remain distinct inputs with fixed preceding context', () => {
  const fixture = buildSessionFixtures([[
    meta(), input('Initial task'), message('assistant', 'Plan A', { phase: 'final_answer' }),
    input('Continue'), message('assistant', 'Completed A', { phase: 'commentary' }), input('Continue'),
  ]]);
  const records = [...fixture.audits].sort((a, b) => a.id.localeCompare(b.id));
  assert.equal(records.length, 2);
  assert.equal(records[0].input[0].text, records[1].input[0].text);
  assert.equal(records[0].turnId, records[1].turnId);
  assert.equal(records[0].context.length, 2);
  assert.equal(records[1].context.length, 4);
  assert.doesNotMatch(JSON.stringify(records[0].context), /Completed A/);
});

test('explicit subagents/internal sources are excluded and sessions cannot mix history', () => {
  const root = [meta(), message('assistant', 'Root plan'), input('Root followup')];
  const other = [meta({ id: '99999999-2222-3333-4444-555555555555' }), input('Other first input')];
  const child = [meta({ source: { subagent: { thread_spawn: {} } } }), message('assistant', 'Child result'), input('Child followup')];
  const fixture = buildSessionFixtures([root, other, child], { minAgentContext: 0 });
  assert.equal(fixture.audits.length, 2);
  const isolated = fixture.audits.find((audit) => audit.input[0].text === 'Other first input');
  assert.deepEqual(isolated.context, []);
  assert.notEqual(isolated.keyId, fixture.audits.find((audit) => audit.input[0].text === 'Root followup').keyId);
  assert.doesNotMatch(JSON.stringify(fixture), /Child/);
});

test('context budget drops old users before Agent text and truncates Unicode only at code points', () => {
  const dialogue = [
    { role: 'assistant', phase: 'final_answer', blocks: [{ kind: 'text', text: '😀'.repeat(31_900) }] },
    { role: 'user', blocks: [{ kind: 'text', text: 'u'.repeat(200) }] },
  ];
  assert.deepEqual(contextWindow(dialogue).map((entry) => entry.role), ['assistant']);
  const truncated = contextWindow([{ ...dialogue[0], blocks: [{ kind: 'text', text: '😀'.repeat(33_000) + 'TAIL' }] }]);
  assert.equal(Array.from(truncated[0].blocks[0].text).length, 32_000);
  assert.equal(truncated[0].truncated, true);
  assert.ok(truncated[0].blocks[0].text.endsWith('TAIL'));
  assert.doesNotMatch(truncated[0].blocks[0].text, /[\uD800-\uDBFF](?![\uDC00-\uDFFF])/);
  const fixture = buildSessionFixtures([[meta(), message('assistant', 'plan'), input('😀'.repeat(35_000))]]);
  assert.equal(Array.from(fixture.audits[0].input[0].text).length, 35_000);
});

test('redaction is consistent and removes credentials, URLs, identity, names and host details', () => {
  const redact = createRedactor();
  redact.register({ cwd: '/home/alice/projects/acme-widget', id: '11111111-2222-3333-4444-555555555555' });
  const source = 'alice acme-widget /home/alice/projects/acme-widget/src/main.rs\nhttps://bob:password@private.example.com/acme-widget?token=private\ngit@private.example.com:alice/acme-widget.git\nalice@private.example.com 10.1.2.3 11111111-2222-3333-4444-555555555555 commit abc123def\nAPI_KEY=not-public password="also-private" Bearer abcdefghijklmnopqrstuvwxyz sk-abcdefghijklmopqrs\n';
  const safe = redact.redact(source);
  assert.equal(redact.redact(source), safe);
  assert.doesNotMatch(safe, /alice|acme-widget|bob|private\.example\.com|10\.1\.2\.3|11111111|abc123def|not-public|also-private|abcdefghijklmopqrs/);
  assert.ok(redact.verify({ safe }));
  assert.throws(() => redact.verify({ unsafe: 'password=not-public' }), /Deidentification/);
  assert.throws(() => redact.verify({ unsafe: 'https://private.example.com' }), /Deidentification/);
  assert.throws(() => redact.verify({ unsafe: 'alice' }), /Deidentification/);
});

test('generated fixtures satisfy the existing query and segmented content contracts', () => {
  const fixture = buildSessionFixtures([[
    meta(), input('Investigate'), message('assistant', 'Start with migration checks.', { phase: 'commentary' }),
    message('assistant', 'Then run database tests.', { phase: 'final_answer' }), input('Continue with 😀数据库'),
  ]]);
  const handle = createDemoService(fixture);
  const list = handle('api/audits', new URLSearchParams({ q: '数据库' }));
  assert.equal(list.status, 200);
  assert.equal(list.value.total, 1);
  const id = list.value.items[0].id;
  const detail = handle('api/audit', new URLSearchParams({ id }));
  assert.equal(detail.status, 200);
  assert.equal(detail.value.context.filter((message) => message.role === 'assistant').length, 2);
  const body = handle('api/audit-content', new URLSearchParams({ id, contentId: detail.value.context[1].content.id }));
  assert.equal(body.status, 200);
  assert.equal(body.value.blocks[0].text, 'Start with migration checks.');
});

test('quoted credentials, unlisted domains, compressed IPv6 and arbitrary paths are deidentified', () => {
  const redactor = createRedactor();
  const source = 'corp.example.company 2001:db8::42 ::1 ~/projects/anonymous-demo/src/main.rs /data/synthetic-workspace/private/file.txt C:\\Private\\folder\\file.txt DEPLOY_PASSWORD="SyntheticPrivate first second" {"api_token":"escaped \\"quoted\\" private value"} call_a123456789abc turn_ffffffffeeeeeeee';
  const safe = redactor.redact(source);
  assert.doesNotMatch(safe, /corp|2001:db8|::1|anonymous-demo|synthetic-workspace|Private|first second|quoted|private value|a123456789abc|ffffffffeeeeeeee/);
  assert.ok(redactor.verify({ safe }));
  for (const raw of ['corp.example.company', '2001:db8::42', '::1', '~/projects/anonymous-demo/src/main.rs', '/data/synthetic-workspace/private/file.txt', 'DEPLOY_PASSWORD="SyntheticPrivate first second"', 'call_a123456789abc', 'turn_ffffffffeeeeeeee']) {
    assert.throws(() => redactor.verify({ raw }), /Deidentification/);
  }
});

test('prose credentials are removed everywhere once learned and domain prefixes do not bypass redaction', () => {
  const redactor = createRedactor();
  const source = '密码：`SyntheticPrivateCredential`\n密钥：SyntheticPrivateCredential\npassword is `SyntheticPrivateCredential`\nRepeated without a label: SyntheticPrivateCredential\nuser.corp.example.company\nreview-env-password-2026';
  redactor.learn(source);
  const safe = redactor.redact(source);
  assert.doesNotMatch(safe, /SyntheticPrivateCredential|corp\.example\.company|review-env-password-2026/);
  assert.ok(redactor.verify({ safe }));
  for (const raw of ['密码：`SyntheticPrivateCredential`', '密钥：SyntheticPrivateCredential', 'password is `SyntheticPrivateCredential`', 'SyntheticPrivateCredential', 'user.corp.example.company', 'review-env-password-2026']) assert.throws(() => redactor.verify({ raw }), /Deidentification/);
});

test('file-like TLDs and standard protocol prefixes cannot hide private hosts', () => {
  const redactor = createRedactor();
  for (const domain of ['corp.internal.sh', 'demo.md', 'demo.rs', 'demo.py', 'demo.zip', 'user.corp.example.company']) {
    assert.notEqual(redactor.redact(domain), domain);
    assert.throws(() => redactor.verify({ text: domain }), /Deidentification/);
  }
  assert.equal(redactor.redact('README.md AGENTS.md SKILL.md'), 'README.md AGENTS.md SKILL.md');
  assert.ok(redactor.verify({ inputKind: 'user.text', text: redactor.redact('user.text') }));
  assert.throws(() => redactor.verify({ text: 'user.text' }), /Deidentification/);
});

test('analysis channels are excluded, and per-message turn metadata never inherits a previous turn', () => {
  const session = parseSessionRows([
    meta(), input('one', 'turn-1'), row('turn_context', { turn_id: 'turn-1', model: 'model-one', effort: 'low' }),
    message('assistant', 'synthetic-private-reasoning', { phase: 'analysis' }),
    message('assistant', 'synthetic-private-thinking', { channel: 'analysis', phase: 'commentary' }),
    message('assistant', 'Visible response', { phase: 'future_phase' }),
    input('two', 'turn-2'), row('turn_context', { turn_id: 'turn-2', model: 'model-two', effort: 'high' }),
    input('three', 'turn-3'),
  ]);
  assert.deepEqual(session.messages.map((entry) => entry.blocks[0].text), ['one', 'Visible response', 'two', 'three']);
  assert.equal(session.messages[1].phase, 'future_phase');
  assert.deepEqual(session.messages[0].requestMetadata, { model: 'model-one', reasoningEffort: 'low' });
  assert.deepEqual(session.messages[2].requestMetadata, { model: 'model-two', reasoningEffort: 'high' });
  assert.deepEqual(session.messages[3].requestMetadata, { model: null, reasoningEffort: null });
  const fixture = buildSessionFixtures([session], { minAgentContext: 0 });
  assert.ok(fixture.audits.every((audit) => /^turn_sample_\d+_\d+$/.test(audit.turnId)));
  assert.doesNotMatch(JSON.stringify(fixture), /turn-[123]|synthetic-private/);
});

test('fixed deidentified local-session sample is read through the mock API with exact Agent dialogue', async () => {
  const fixture = JSON.parse(await readFile(new URL('./fixtures/codex-sessions.json', import.meta.url), 'utf8'));
  assert.equal(fixture.audits.length, 40);
  assert.ok(fixture.keys.length >= 3);
  assert.ok(fixture.audits.every((audit) => audit.context.some((message) => message.role === 'assistant')));
  assert.ok(fixture.audits.some((audit) => audit.context.some((message) => message.phase === 'commentary')));
  assert.ok(fixture.audits.some((audit) => audit.context.some((message) => message.phase === 'final_answer')));
  assert.ok(createRedactor().verify(fixture));
  const messages = new Map();
  const service = createDemoService(fixture);
  const read = (id, contentId) => {
    const blocks = [];
    let cursor;
    do {
      const result = service('api/audit-content', new URLSearchParams({ id, contentId, ...(cursor ? { cursor } : {}) }));
      assert.equal(result.status, 200);
      for (const block of result.value.blocks) {
        if (block.kind === 'text') blocks[block.index] = (blocks[block.index] || '') + block.text;
        else blocks[block.index] = `[${block.mediaType}]`;
      }
      cursor = result.value.nextCursor;
    } while (cursor);
    return blocks;
  };
  for (const audit of fixture.audits) {
    const detail = service('api/audit', new URLSearchParams({ id: audit.id }));
    assert.equal(detail.status, 200);
    assert.deepEqual(read(audit.id, detail.value.input.id), audit.input.map((block) => block.kind === 'text' ? block.text : `[${block.mediaType}]`));
    assert.equal(detail.value.context.length, audit.context.length);
    for (let index = 0; index < audit.context.length; index++) {
      const original = audit.context[index], descriptor = detail.value.context[index];
      assert.equal(descriptor.role, original.role);
      assert.equal(descriptor.phase, original.phase ?? null);
      assert.deepEqual(read(audit.id, descriptor.content.id), original.blocks.map((block) => block.kind === 'text' ? block.text : `[${block.mediaType}]`));
      const previous = messages.get(original.id);
      if (previous) assert.deepEqual(original, previous);
      messages.set(original.id, original);
    }
  }
});
