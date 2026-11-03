// Minimal argument parser shared by the rig's Node scripts.
// `--name value`, `--name=value`, and bare `--flag` (names listed in `bools` never take a value).
export function parseArgs(argv, bools = []) {
  const pos = [];
  const flags = {};
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (!a.startsWith('--')) {
      pos.push(a);
      continue;
    }
    const eq = a.indexOf('=');
    if (eq > 0) {
      flags[a.slice(2, eq)] = a.slice(eq + 1);
    } else if (bools.includes(a.slice(2)) || i + 1 >= argv.length || argv[i + 1].startsWith('--')) {
      flags[a.slice(2)] = true;
    } else {
      flags[a.slice(2)] = argv[++i];
    }
  }
  return { pos, flags };
}
