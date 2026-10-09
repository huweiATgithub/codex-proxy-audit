import { PAGE_SIZES } from '../contracts/audit.js';
export { PAGE_SIZES };
export function searchPattern(query) {
  const literal = query.replace(/[.*+?^${}()|[\]\\]/g, '\\$&').replace(/[a-z]/gi, (letter) => `[${letter.toLowerCase()}${letter.toUpperCase()}]`);
  return new RegExp(literal, 'gu');
}
