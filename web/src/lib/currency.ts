/** Money helpers for the spend budget. The wire unit is micro-USD (1e-6 USD,
 *  matching the backend `*_micro_usd` columns); the UI shows and edits dollars. */

const MICRO_PER_USD = 1_000_000;

/** Format micro-USD as `$X.XX`. */
export function formatUSD(microUsd: number): string {
  const usd = microUsd / MICRO_PER_USD;
  return `$${usd.toLocaleString(undefined, {
    minimumFractionDigits: 2,
    maximumFractionDigits: 2,
  })}`;
}

/** Convert a dollar amount to micro-USD, rounded to the nearest micro. */
export function usdToMicro(usd: number): number {
  return Math.round(usd * MICRO_PER_USD);
}

/** Convert micro-USD to whole dollars for an input field's default value. */
export function microToUsd(microUsd: number): number {
  return microUsd / MICRO_PER_USD;
}
