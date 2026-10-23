import { writeFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import { performance } from 'node:perf_hooks';

import { DaemonClient } from '../out/src/daemonClient.js';

const queries = ['class ', 'def ', 'function ', 'async ', 'TODO', '__cctx_absent_71d83__'];

const output = process.argv[2];
if (!output || !process.argv[3]) {
  throw new Error('usage: node scripts/bench-native.mjs OUTPUT ROOT');
}
const root = resolve(process.argv[3]);
const cancellation = {
  isCancellationRequested: false,
  onCancellationRequested: () => ({ dispose: () => undefined }),
};
const client = new DaemonClient(() => ({
  socketPath: '',
  autoStartService: false,
  requestTimeoutMs: 40_000,
}));

function summarize(values) {
  const sorted = [...values].sort((left, right) => left - right);
  const percentile = (percent) =>
    sorted[Math.ceil((sorted.length * percent) / 100) - 1];
  return {
    samples: sorted.length,
    mean_ms: sorted.reduce((sum, value) => sum + value, 0) / sorted.length,
    median_ms:
      sorted.length % 2 === 0
        ? (sorted[sorted.length / 2 - 1] + sorted[sorted.length / 2]) / 2
        : sorted[Math.floor(sorted.length / 2)],
    p95_ms: percentile(95),
    p99_ms: percentile(99),
    max_ms: sorted.at(-1),
  };
}

try {
  const workspaces = await client.request(
    { root: null, name: 'code_workspaces', arguments: {}, compact: true },
    cancellation,
  );
  if (!Array.isArray(workspaces) || !workspaces.some((item) => item.root === root)) {
    throw new Error(`root is not registered: ${root}`);
  }
  const blocks = [];
  for (let block = 0; block < 5; block += 1) {
    const values = [];
    for (let round = 0; round < 100; round += 1) {
      for (const pattern of queries) {
        const started = performance.now();
        await client.request(
          {
            root,
            name: 'code_grep',
            arguments: { pattern, limit: 20 },
            compact: true,
          },
          cancellation,
        );
        values.push(performance.now() - started);
      }
    }
    blocks.push(summarize(values));
  }
  const median = (values) => {
    const sorted = [...values].sort((left, right) => left - right);
    return sorted[Math.floor(sorted.length / 2)];
  };
  const report = {
    schema: 1,
    path: 'vscode-native-adapter',
    root,
    queries,
    blocks,
    summary: {
      median_block_mean_ms: median(blocks.map((block) => block.mean_ms)),
      median_block_median_ms: median(blocks.map((block) => block.median_ms)),
      median_block_p95_ms: median(blocks.map((block) => block.p95_ms)),
      worst_max_ms: Math.max(...blocks.map((block) => block.max_ms)),
    },
  };
  await writeFile(output, `${JSON.stringify(report, null, 2)}\n`, { flag: 'wx' });
  console.log(JSON.stringify(report.summary, null, 2));
} finally {
  client.dispose();
}
