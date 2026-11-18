// Precompresses the built assets (brotli + gzip, next to each file), so the
// server sends them without compressing at run time. Node's zlib only: no
// extra packages.
import { brotliCompressSync, gzipSync, constants } from 'node:zlib';
import { readdirSync, readFileSync, writeFileSync, statSync } from 'node:fs';
import { join, extname } from 'node:path';

const dir = process.env.FLWRS_WEB_OUT_DIR ?? 'dist';
const compressible = new Set(['.html', '.js', '.css', '.svg', '.json', '.txt', '.map']);
const minBytes = 1024;

function* files(d) {
  for (const name of readdirSync(d)) {
    const path = join(d, name);
    if (statSync(path).isDirectory()) yield* files(path);
    else yield path;
  }
}

for (const path of [...files(dir)]) {
  if (!compressible.has(extname(path))) continue;
  const raw = readFileSync(path);
  if (raw.length < minBytes) continue;
  const br = brotliCompressSync(raw, {
    params: {
      [constants.BROTLI_PARAM_QUALITY]: constants.BROTLI_MAX_QUALITY,
      [constants.BROTLI_PARAM_SIZE_HINT]: raw.length,
    },
  });
  const gz = gzipSync(raw, { level: 9 });
  // Only kept when they actually save space.
  if (br.length < raw.length) writeFileSync(`${path}.br`, br);
  if (gz.length < raw.length) writeFileSync(`${path}.gz`, gz);
}
