const warned = new Set<string>();

/**
 * `TOKENREIN_<name>` from the environment, or the name used before the product was renamed
 * (read with a deprecation warning until v1.1).
 */
export function envVar(name: string): string | undefined {
  const value = process.env[`TOKENREIN_${name}`];
  if (value) return value;
  const old = `TOKENLENS_${name}`; // pre-rename name
  const legacy = process.env[old];
  if (legacy && !warned.has(old)) {
    warned.add(old);
    process.emitWarning(`${old} is deprecated; set TOKENREIN_${name}`, 'DeprecationWarning');
  }
  return legacy || undefined;
}
