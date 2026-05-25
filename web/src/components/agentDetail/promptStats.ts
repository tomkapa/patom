/** Line/token/char counts shown under the system-prompt editor footer.
 *  Token count is a rough byte/4 estimate — accurate enough for an
 *  at-a-glance counter; we don't want a tokenizer in the FE bundle. */
export function describePrompt(value: string): {
  lines: number;
  tokens: number;
  chars: number;
} {
  const chars = value.length;
  const lines = chars === 0 ? 0 : value.split("\n").length;
  const tokens = Math.max(0, Math.round(chars / 4));
  return { lines, tokens, chars };
}
