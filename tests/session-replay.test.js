import test from 'node:test';
import assert from 'node:assert/strict';
import { matchFixtureTargets, sessionFixtureRequests } from '../scripts/seed-session-fixtures.js';

const block = (text, inputKind = 'user.text') => ({ kind: 'text', text, inputKind });
const audit = (id, messageId, threadId, context = [], extra = {}) => ({ id, messageId, threadId, sessionId: `session-${threadId}`, turnId: 'same-turn', input: [block('请继续')], context, ...extra });

test('replay preserves message identity and assistant phase across complete history requests', () => {
  const document = { audits: [
    audit('audit-a', 'user-a', 'thread-a'),
    audit('audit-b', 'user-b', 'thread-a', [
      { id: 'user-a', role: 'user', blocks: [block('请继续')] },
      { id: 'agent-a', role: 'assistant', phase: 'commentary', blocks: [block('先核对契约')] },
      { id: 'agent-b', role: 'assistant', phase: 'final_answer', blocks: [block('方案已经准备完成')] },
    ]),
    audit('audit-c', 'user-a', 'thread-b'),
  ] };
  const first = sessionFixtureRequests(document), again = sessionFixtureRequests(document);
  assert.deepEqual(first, again);
  assert.equal(first.expectedInputCount, 3);
  assert.deepEqual(first.requests[1].body.input.map((message) => message.id), ['user-a', 'agent-a', 'agent-b', 'user-b']);
  assert.deepEqual(first.requests[1].body.input.filter((message) => message.role === 'assistant').map((message) => message.phase), ['commentary', 'final_answer']);
  assert.equal(first.requests[1].body.input[0].internal_chat_message_metadata_passthrough.turn_id, 'same-turn');
  assert.equal(first.requests[1].body.input.at(-1).internal_chat_message_metadata_passthrough.turn_id, 'same-turn');
  assert.equal('previous_response_id' in first.requests[1].body, false);
  assert.deepEqual(JSON.parse(first.requests[1].body.client_metadata['x-codex-turn-metadata']), { thread_source: 'user', thread_id: 'thread-a', session_id: 'session-thread-a' });
});

test('previous user messages supply context without producing extra audit records or invented turn metadata', () => {
  const { requests, expectedInputCount } = sessionFixtureRequests({ audits: [audit('audit-a', 'user-a', 'thread-a', [
    { id: 'historical-user', role: 'user', blocks: [{ kind: 'text', text: '原先的请求' }] },
    { id: 'agent-a', role: 'assistant', blocks: [block('这是 Agent 的原文')] },
  ], { turnId: null })] });
  assert.equal(expectedInputCount, 1);
  assert.deepEqual(requests[0].body.input[0].internal_chat_message_metadata_passthrough, {});
  assert.equal('turn_id' in requests[0].body.input.at(-1).internal_chat_message_metadata_passthrough, false);
  assert.equal('phase' in requests[0].body.input[1], false);
});

test('repeated fixture requests count separately even with identical message identities', () => {
  const repeated = audit('audit-a', 'user-a', 'thread-a');
  const { requests, expectedInputCount } = sessionFixtureRequests({ audits: [repeated, repeated] });
  assert.equal(expectedInputCount, 2);
  assert.equal(requests.length, 2);
  const observations = requests.map((request, index) => ({ detail: { id: `stored-${index}`, threadId: request.threadId, sessionId: request.sessionId, turnId: request.turnId }, input: request.input, context: request.context }));
  assert.equal(new Set(matchFixtureTargets(requests, observations).map(({ detail }) => detail.id)).size, 2);
});

test('missing stable identities and unsupported fixture bodies fail at the replay boundary', () => {
  assert.throws(() => sessionFixtureRequests({ audits: [audit('audit-a', undefined, 'thread-a')] }));
  assert.throws(() => sessionFixtureRequests({ audits: [audit('audit-a', 'user-a', 'thread-a', [], { input: [{ kind: 'nontext', mediaType: 'image' }] })] }));
  assert.throws(() => sessionFixtureRequests({ audits: [audit('audit-a', 'user-a', 'thread-a', [{ id: 'reasoning', role: 'assistant', phase: 'analysis', blocks: [block('excluded reasoning')] }])] }));
});

test('source order is replayed before later inputs without sending source timestamps', () => {
  const { requests } = sessionFixtureRequests({ audits: [
    audit('later', 'user-b', 'thread-a', [], { firstObservedAt: '2026-10-09T02:00:00.000Z' }),
    audit('earlier', 'user-a', 'thread-a', [], { firstObservedAt: '2026-10-09T01:00:00.000Z' }),
  ] });
  assert.deepEqual(requests.map((request) => request.auditId), ['earlier', 'later']);
  assert.equal(JSON.stringify(requests.map((request) => request.body)).includes('2026-10-09'), false);
});

test('verification distinguishes same-turn identical inputs by complete ordered Agent context', () => {
  const { requests } = sessionFixtureRequests({ audits: [
    audit('audit-a', 'user-a', 'thread-a', [{ id: 'agent-a', role: 'assistant', phase: 'commentary', blocks: [block('第一版方案')] }]),
    audit('audit-b', 'user-b', 'thread-a', [{ id: 'agent-b', role: 'assistant', phase: 'final_answer', blocks: [block('第二版方案')] }]),
  ] });
  const observations = requests.map((request, index) => ({ detail: { id: `stored-${index}`, threadId: request.threadId, sessionId: request.sessionId, turnId: request.turnId }, input: request.input, context: request.context }));
  assert.deepEqual(matchFixtureTargets(requests, observations.reverse()).map(({ detail }) => detail.id), ['stored-0', 'stored-1']);
  assert.throws(() => matchFixtureTargets(requests, observations.map((observation) => ({ ...observation, context: observation.context.map((message) => ({ ...message, phase: null })) }))));
  assert.throws(() => matchFixtureTargets(requests, [observations[0]]));
});
