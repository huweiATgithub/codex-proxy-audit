import { createApp } from 'vue';
import '@codex-proxy/ui/styles.css';
import './style.css';
import { host } from './api/host.js';
import { createAuditClient } from './api/client.js';
import App from './Plugin.vue';

createApp(App).provide('auditClient', createAuditClient(host)).mount('#app');
