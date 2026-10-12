<script setup>
import { computed, inject, ref } from 'vue';
import { BaseButton } from '@codex-proxy/ui';
import { Bot, ChevronDown, ChevronUp, User } from '@lucide/vue';
import ContentBlocks from './ContentBlocks.vue';
import { useRequestMetadata } from '../state/request-metadata.js';

const props = defineProps({ audit: { type: Object, required: true }, query: { type: String, default: '' } });
const { state: requestMetadata, load: loadRequestMetadata } = useRequestMetadata(inject('auditClient'), () => props.audit.id);
const contextOpen = ref(true);
const showEarlier = ref(false);
const messages = computed(() => showEarlier.value ? props.audit.context : props.audit.context.slice(-3));
const metadata = computed(() => [
  ['Client Key', `${props.audit.keyName || '名称未知'} · ${props.audit.keyId}`],
  ['记录时间', `${props.audit.recordedAtDisplay} · ${props.audit.timeZone}`],
  ['审计 ID', props.audit.id], ['来源请求 ID', props.audit.requestId || '未取得'],
  ['Thread ID', props.audit.threadId || '未取得'], ['Session ID', props.audit.sessionId || '未取得'],
  ['Turn ID', props.audit.turnId || '未取得'],
]);
</script>

<template>
  <div class="detail-scroll">
    <section class="input-card">
      <h2><User :size="15" aria-hidden="true" />用户输入</h2>
      <ContentBlocks :audit-id="audit.id" :content="audit.input" :query="query" />
    </section>
    <section class="context-section">
      <div class="section-heading">
        <div><h2>前序对话</h2><span class="message-count">{{ audit.context.length }} 条 · 按原会话顺序</span></div>
        <BaseButton variant="ghost" size="sm" :aria-expanded="contextOpen" @click="contextOpen = !contextOpen">
          {{ contextOpen ? '收起' : '展开' }}<component :is="contextOpen ? ChevronUp : ChevronDown" :size="14" />
        </BaseButton>
      </div>
      <div v-if="contextOpen" class="context-messages">
        <p v-if="!audit.context.length" class="content-note">没有保存的前序对话。</p>
        <BaseButton v-if="audit.context.length > 3 && !showEarlier" class="earlier-button" variant="secondary" size="sm" @click="showEarlier = true">展开更早的 {{ audit.context.length - 3 }} 条消息<ChevronDown :size="14" /></BaseButton>
        <article v-for="message in messages" :key="message.id" class="message" :class="{ 'agent-message': message.role === 'assistant' }">
          <div class="message-heading">
            <span class="message-icon"><component :is="message.role === 'assistant' ? Bot : User" :size="14" /></span>
            <strong>{{ message.role === 'assistant' ? 'Agent' : '用户' }}</strong>
            <span v-if="message.phase" class="message-phase">{{ message.phase === 'commentary' ? '过程说明' : message.phase === 'final_answer' ? '最终答复' : message.phase }}</span>
          </div>
          <p v-if="message.truncated" class="content-note">[仅保存了原文末尾]</p>
          <ContentBlocks :audit-id="audit.id" :content="message.content" />
        </article>
        <p v-if="audit.context.length" class="snapshot-boundary">前序对话到此为止 · 后续回复不属于本条输入的上下文</p>
      </div>
    </section>
    <section class="metadata" aria-labelledby="metadata-heading">
      <h3 id="metadata-heading">记录信息</h3>
      <dl>
        <div v-for="[label, value] in metadata" :key="label" class="metadata-field">
          <dt>{{ label }}</dt><dd><time v-if="label === '记录时间'" :datetime="audit.recordedAt">{{ value }}</time><template v-else>{{ value }}</template></dd>
        </div>
        <div class="metadata-field">
          <dt>输入类型</dt><dd><div v-for="(kind, index) in audit.inputKinds" :key="index">块 {{ index + 1 }} · {{ kind || '未提供' }}</div><span v-if="!audit.inputKinds.length">无内容块</span></dd>
        </div>
        <div class="metadata-field metadata-wide">
          <dt>仓库</dt><dd>
            <span v-if="audit.repositories === null">未取得</span><span v-else-if="!audit.repositories.length">未报告仓库</span>
            <div v-for="(repository, index) in audit.repositories" :key="index" class="repository-info">
              <div>{{ repository.rootPath }}</div>
              <div v-for="(remote, remoteIndex) in repository.remotes" :key="remoteIndex">{{ remote.name }} · {{ remote.url }}</div>
              <div v-if="repository.remotes === null">Remote URLs 未取得</div><div v-else-if="!repository.remotes.length">无 Remote</div>
            </div>
          </dd>
        </div>
      </dl>
      <p class="metadata-note">仓库信息为来源请求的快照。</p>
      <div class="request-metadata" :aria-busy="requestMetadata.kind === 'loading'">
        <BaseButton v-if="requestMetadata.kind === 'idle' || requestMetadata.kind === 'loading'" variant="secondary" size="sm" :disabled="requestMetadata.kind === 'loading'" @click="loadRequestMetadata">{{ requestMetadata.kind === 'loading' ? '正在加载请求信息…' : '加载请求信息' }}</BaseButton>
        <div v-if="requestMetadata.kind === 'error'" class="inline-error" role="alert"><span>{{ requestMetadata.error.message }}</span><BaseButton size="sm" variant="ghost" @click="loadRequestMetadata">重试请求信息</BaseButton></div>
        <p v-else-if="requestMetadata.kind === 'missing'" class="metadata-note" role="status">来源请求记录不可用。</p>
        <template v-else-if="requestMetadata.kind === 'ready'">
          <dl>
            <div class="metadata-field"><dt>请求模型</dt><dd>{{ requestMetadata.data.model || '未取得' }}</dd></div>
            <div class="metadata-field"><dt>推理强度</dt><dd>{{ requestMetadata.data.reasoningEffort || '未取得' }}</dd></div>
            <div class="metadata-field metadata-wide"><dt>User-Agent</dt><dd>{{ requestMetadata.data.userAgent || '未取得' }}</dd></div>
          </dl>
          <p class="metadata-note">模型、推理强度与 UA 取自宿主记录。</p>
        </template>
      </div>
    </section>
  </div>
</template>
