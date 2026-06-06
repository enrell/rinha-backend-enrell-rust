import http from 'k6/http';
import { check } from 'k6';

const baseUrl = __ENV.BENCH_URL || 'http://127.0.0.1:9999';
const payload = JSON.stringify({
    id: 'tx-smoke',
    transaction: { amount: 41.12, installments: 2, requested_at: '2026-03-11T18:45:53Z' },
    customer: { avg_amount: 82.24, tx_count_24h: 3, known_merchants: ['MERC-016'] },
    merchant: { id: 'MERC-016', mcc: '5411', avg_amount: 60.25 },
    terminal: { is_online: false, card_present: true, km_from_home: 29.23 },
    last_transaction: null,
});

export const options = {
    vus: 1,
    iterations: 3,
    thresholds: {
        checks: ['rate==1.0'],
        http_req_failed: ['rate==0.0'],
    },
};

export default function () {
    const ready = http.get(`${baseUrl}/ready`, { timeout: '2s' });
    check(ready, { 'ready 2xx': (r) => r.status >= 200 && r.status < 300 });

    const res = http.post(`${baseUrl}/fraud-score`, payload, {
        headers: { 'Content-Type': 'application/json' },
        timeout: '2s',
    });
    check(res, {
        'fraud-score 200': (r) => r.status === 200,
        'approved bool': (r) => typeof JSON.parse(r.body).approved === 'boolean',
        'fraud_score number': (r) => typeof JSON.parse(r.body).fraud_score === 'number',
    });
}