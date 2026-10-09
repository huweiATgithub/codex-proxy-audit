import { readFileSync, readdirSync, statSync, writeFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { endpoints } from '../contracts/audit.js';

const directory = resolve('dist/plugin');
const files = readdirSync(directory).filter((file) => file !== 'management.json');
const html = readFileSync(resolve(directory, 'index.html'), 'utf8');
if (/type=["']module|modulepreload/.test(html)) throw new Error('Host requires classic scripts.');
if (files.some((file) => !/^[\w.-]+$/.test(file) || !statSync(resolve(directory, file)).isFile())) throw new Error('Unexpected plugin resource path.');
if (files.length > 128 || files.some((file) => statSync(resolve(directory, file)).size > 1024 * 1024)
  || files.reduce((size, file) => size + statSync(resolve(directory, file)).size, 0) > 8 * 1024 * 1024) throw new Error('Plugin resource budget exceeded.');
const script = readFileSync(resolve(directory, 'app.js'), 'utf8');
if (script.includes('/__demo/') || script.includes('req_demo_') || script.includes('thread_demo_')) throw new Error('Demo transport or fixtures leaked into plugin build.');
const registration = {
  routes: Object.values(endpoints).map(({ path }) => ({ method: 'GET', path, request_content_types: [], response_content_types: ['application/json'] })),
  resources: files.map((path) => ({ path, public: false })),
  pages: [{ id: 'prompt-audit', title: 'Prompt 审计', description: '检索用户输入，查看原文与前序对话。', entry: 'index.html', icon: null }],
  callbacks: [],
};
writeFileSync(resolve(directory, 'management.json'), JSON.stringify(registration, null, 2) + '\n');
console.log('Plugin resources verified; management.json generated for the backend registration.');
