<script setup>
import { computed } from 'vue';
import { searchPattern } from '../audit.js';

const props = defineProps({ text: { type: String, required: true }, query: { type: String, default: '' } });
const segments = computed(() => {
  if (!props.query) return [{ text: props.text, match: false }];
  const result = [];
  let offset = 0;
  for (const match of props.text.matchAll(searchPattern(props.query))) {
    result.push({ text: props.text.slice(offset, match.index), match: false }, { text: match[0], match: true });
    offset = match.index + match[0].length;
  }
  result.push({ text: props.text.slice(offset), match: false });
  return result;
});
</script>

<template>
  <template v-for="(segment, index) in segments" :key="index"><mark v-if="segment.match">{{ segment.text }}</mark><template v-else>{{ segment.text }}</template></template>
</template>
