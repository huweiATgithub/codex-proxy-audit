import { invalidResponse } from './client.js';

// Parse a response into an ordered continuation before committing it to the UI.
export function appendContent(previous, page, descriptor) {
  const blocks = previous.map((block) => ({ ...block }));
  for (const segment of page.blocks) {
    if (segment.kind === 'text' && !segment.text && segment.totalCharacters > 0) throw invalidResponse();
    if (segment.index > blocks.length) throw invalidResponse();
    const last = blocks.at(-1);
    if (segment.index === blocks.length) {
      if (last?.kind === 'text' && Array.from(last.text).length !== last.totalCharacters) throw invalidResponse();
      if (segment.kind === 'text' && segment.offset !== 0) throw invalidResponse();
      blocks.push({ ...segment });
    } else {
      if (segment.index !== blocks.length - 1 || segment.kind !== 'text' || last.kind !== 'text'
        || segment.offset !== Array.from(last.text).length || segment.totalCharacters !== last.totalCharacters || !segment.text) throw invalidResponse();
      last.text += segment.text;
    }
    const current = blocks.at(-1);
    if (current.kind === 'text' && Array.from(current.text).length > current.totalCharacters) throw invalidResponse();
  }
  const characterCount = blocks.reduce((total, block) => total + (block.kind === 'text' ? Array.from(block.text).length : 0), 0);
  const declaredCharacters = blocks.reduce((total, block) => total + (block.kind === 'text' ? block.totalCharacters : 0), 0);
  if (declaredCharacters > descriptor.characterCount || characterCount > descriptor.characterCount || blocks.length > descriptor.blockCount) throw invalidResponse();
  if (!page.nextCursor && (characterCount !== descriptor.characterCount || blocks.length !== descriptor.blockCount
    || blocks.some((block) => block.kind === 'text' && Array.from(block.text).length !== block.totalCharacters))) throw invalidResponse();
  return { blocks, characterCount, nextCursor: page.nextCursor };
}
