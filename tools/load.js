import http from 'k6/http';
export const options = {
  vus: __ENV.VUS ? parseInt(__ENV.VUS) : 64,
  duration: __ENV.DUR || '15s',
  discardResponseBodies: true,
  summaryTrendStats: ['med','p(99)'],
};
const BASE = __ENV.BASE;
const IDX = __ENV.IDX || 'bench_logs';
const KIND = __ENV.KIND || 'search';
const Q = [
  JSON.stringify({query:{match_all:{}},size:10}),
  JSON.stringify({query:{term:{region:'eu-west-1'}},size:10}),
  JSON.stringify({size:0,aggs:{by_status:{terms:{field:'status',size:10}}}}),
];
const H = {headers:{'Content-Type':'application/json'}};
export default function () {
  if (KIND === 'root') { http.get(`${BASE}/`); return; }
  http.post(`${BASE}/${IDX}/_search`, Q[__ITER % Q.length], H);
}
