import http from 'k6/http';
import { SharedArray } from 'k6/data';
import { Counter } from 'k6/metrics';
import exec from 'k6/execution';

const dataPath = __ENV.BENCH_TEST_DATA || '/testdata/test-data.json';
const baseUrl = __ENV.BENCH_URL || 'http://127.0.0.1:9999';
const testData = new SharedArray('accuracy-test-data', function () {
    return JSON.parse(open(dataPath)).entries;
});
const total = Math.min(Number(__ENV.ACC_LIMIT || testData.length), testData.length);

const tpCount = new Counter('tp_count');
const tnCount = new Counter('tn_count');
const fpCount = new Counter('fp_count');
const fnCount = new Counter('fn_count');
const errorCount = new Counter('error_count');

export const options = {
    summaryTrendStats: ['p(99)'],
    scenarios: {
        accuracy: {
            executor: 'shared-iterations',
            vus: Number(__ENV.ACC_VUS || 64),
            iterations: total,
            maxDuration: __ENV.ACC_MAX_DURATION || '120s',
        },
    },
};

export default function () {
    const entry = testData[exec.scenario.iterationInTest];
    const res = http.post(`${baseUrl}/fraud-score`, JSON.stringify(entry.request), {
        headers: { 'Content-Type': 'application/json' },
        timeout: '2s',
    });

    if (res.status !== 200) {
        errorCount.add(1);
        return;
    }

    const body = JSON.parse(res.body);
    if (body.approved === entry.expected_approved) {
        if (body.approved) tnCount.add(1);
        else tpCount.add(1);
    } else if (body.approved) {
        fnCount.add(1);
    } else {
        fpCount.add(1);
    }
}

export function handleSummary(data) {
    const metricCount = (name) => data.metrics[name]?.values.count || 0;
    const tp = metricCount('tp_count');
    const tn = metricCount('tn_count');
    const fp = metricCount('fp_count');
    const fn = metricCount('fn_count');
    const errors = metricCount('error_count');
    const p99 = data.metrics.http_req_duration.values['p(99)'] || 0;
    return {
        stdout: [
            '',
            '=== bench/accuracy ===',
            `entries: ${total}`,
            `tp=${tp} tn=${tn} fp=${fp} fn=${fn} errors=${errors}`,
            `p99=${p99.toFixed(3)}ms`,
            '',
        ].join('\n'),
    };
}
