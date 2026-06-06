import http from 'k6/http';
import { SharedArray } from 'k6/data';
import exec from 'k6/execution';

const limit = Number(__ENV.BENCH_MINI_LIMIT || 800);
const dataPath = __ENV.BENCH_TEST_DATA || '/testdata/test-data.json';
const baseUrl = __ENV.BENCH_URL || 'http://127.0.0.1:9999';

const entries = new SharedArray('mini-test-data', function () {
    const doc = JSON.parse(open(dataPath));
    return doc.entries.slice(0, limit);
});

const duration = __ENV.BENCH_DURATION || '12s';
const rate = Number(__ENV.BENCH_RATE || 70);

export const options = {
    summaryTrendStats: ['p(99)'],
    scenarios: {
        mini: {
            executor: 'constant-arrival-rate',
            rate,
            timeUnit: '1s',
            duration,
            preAllocatedVUs: 30,
            maxVUs: 80,
            gracefulStop: '2s',
        },
    },
};

export default function () {
    const idx = exec.scenario.iterationInTest % entries.length;
    const entry = entries[idx];
    http.post(`${baseUrl}/fraud-score`, JSON.stringify(entry.request), {
        headers: { 'Content-Type': 'application/json' },
        timeout: '2s',
    });
}

export function handleSummary(data) {
    const dur = data.metrics.http_req_duration.values;
    const p99 = dur['p(99)'] ?? 0;
    const failedRate = data.metrics.http_req_failed?.values?.rate ?? 0;
    const count = data.metrics.http_reqs?.values?.count ?? 0;

    const out = [
        '',
        '=== bench/mini (accuracy sample) ===',
        `dataset: first ${entries.length} entries from test-data.json`,
        `target: ${rate}/s for ${duration}`,
        `requests: ${count}  http_failed: ${(failedRate * 100).toFixed(2)}%  p99: ${p99.toFixed(3)}ms`,
        'compare TP/TN/FP/FN with: ./bench/run.sh mini-report (or official test.js)',
        '',
    ].join('\n');

    return { stdout: out };
}