import http from 'k6/http';

const payload = JSON.stringify({
    id: 'tx-bench-001',
    transaction: {
        amount: 384.88,
        installments: 3,
        requested_at: '2026-03-11T20:23:35Z',
    },
    customer: {
        avg_amount: 769.76,
        tx_count_24h: 3,
        known_merchants: ['MERC-009', 'MERC-001'],
    },
    merchant: {
        id: 'MERC-001',
        mcc: '5912',
        avg_amount: 298.95,
    },
    terminal: {
        is_online: false,
        card_present: true,
        km_from_home: 13.7090520965,
    },
    last_transaction: {
        timestamp: '2026-03-11T14:58:35Z',
        km_from_current: 18.8626479774,
    },
});

const duration = __ENV.BENCH_DURATION || '15s';
const rate = Number(__ENV.BENCH_RATE || 900);
const baseUrl = __ENV.BENCH_URL || 'http://127.0.0.1:9999';

export const options = {
    summaryTrendStats: ['avg', 'p(99)'],
    scenarios: {
        quick: {
            executor: 'constant-arrival-rate',
            rate,
            timeUnit: '1s',
            duration,
            preAllocatedVUs: 80,
            maxVUs: 150,
            gracefulStop: '2s',
        },
    },
    thresholds: {
        http_req_failed: ['rate<0.01'],
    },
};

export default function () {
    http.post(`${baseUrl}/fraud-score`, payload, {
        headers: { 'Content-Type': 'application/json' },
        timeout: '2s',
        tags: { name: 'fraud-score' },
    });
}

export function handleSummary(data) {
    const dur = data.metrics.http_req_duration.values;
    const p99 = dur['p(99)'] ?? 0;
    const avg = dur.avg ?? 0;
    const failedRate = data.metrics.http_req_failed?.values?.rate ?? 0;
    const count = data.metrics.http_reqs?.values?.count ?? 0;
    const elapsed = (data.state.testRunDurationMs || 1) / 1000;
    const rps = count / elapsed;

    const K = 1000;
    const T_MAX = 1000;
    const P99_MIN = 1;
    const P99_MAX = 2000;
    let scoreP99;
    if (p99 > P99_MAX) {
        scoreP99 = -3000;
    } else {
        scoreP99 = Math.min(3000, K * Math.log10(T_MAX / Math.max(p99, P99_MIN)));
    }

    const out = [
        '',
        '=== bench/quick (perf) ===',
        `target: ${rate}/s for ${duration}  |  stack limits: 1 CPU / 350MB (lb+api1+api2)`,
        `bench client limits: 1 CPU / 512MB`,
        `requests: ${count}  elapsed: ${elapsed.toFixed(1)}s  achieved_rps: ${rps.toFixed(0)}`,
        `http_failed: ${(failedRate * 100).toFixed(2)}%`,
        `latency  avg: ${avg.toFixed(3)}ms   p99: ${p99.toFixed(3)}ms`,
        `est. score_p99 (fórmula oficial): ${scoreP99.toFixed(0)}  (oficial usa 120s @ 900/s)`,
        '',
    ].join('\n');

    return { stdout: out };
}