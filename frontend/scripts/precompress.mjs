import { readdir, readFile, writeFile } from 'node:fs/promises';
import { extname, join } from 'node:path';
import { promisify } from 'node:util';
import { brotliCompress, constants, gzip } from 'node:zlib';

const gzipAsync = promisify(gzip);
const brotliAsync = promisify(brotliCompress);
const compressible = new Set([
  '.css',
  '.html',
  '.js',
  '.json',
  '.svg',
  '.txt',
  '.webmanifest',
  '.xml',
]);

async function filesBelow(directory) {
  const entries = await readdir(directory, { withFileTypes: true });
  const paths = [];
  for (const entry of entries) {
    const path = join(directory, entry.name);
    if (entry.isDirectory()) paths.push(...await filesBelow(path));
    else if (entry.isFile()) paths.push(path);
  }
  return paths;
}

const outputDirectory = process.argv[2];
if (!outputDirectory) throw new Error('usage: node scripts/precompress.mjs <directory>');

for (const path of await filesBelow(outputDirectory)) {
  if (path.endsWith('.br') || path.endsWith('.gz') || !compressible.has(extname(path))) continue;
  const bytes = await readFile(path);
  const [gzipBytes, brotliBytes] = await Promise.all([
    gzipAsync(bytes, { level: 9 }),
    brotliAsync(bytes, {
      params: { [constants.BROTLI_PARAM_QUALITY]: 11 },
    }),
  ]);
  await Promise.all([
    writeFile(`${path}.gz`, gzipBytes),
    writeFile(`${path}.br`, brotliBytes),
  ]);
}
