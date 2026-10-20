import assert from 'node:assert/strict';
import test from 'node:test';
import {parseMatrix, testResults, check} from './check-matrix.mjs';

const matrix = `
| Feature | Status | Last passed | Test |
| --- | --- | --- | --- |
| Create | works | v0.9.22 | \`a.spec.mjs › creates\` |
| Phone | works | v0.9.22 | \`a.spec.mjs › narrow › fits\`; \`a.spec.mjs › creates\` |
| Budget | partial | v0.9.22 | \`a.spec.mjs › budget\` (refusal only) |
| Account | missing | — | not offered |
`;
const report = status => ({suites: [{title: 'a.spec.mjs', file: 'a.spec.mjs',
  specs: [{title: 'creates', file: 'a.spec.mjs', tests: [{expectedStatus: 'passed', results: [{status}]}]},
    {title: 'budget', file: 'a.spec.mjs', tests: [{expectedStatus: 'passed', results: [{status: 'failed'}]}]}],
  suites: [{title: 'narrow', file: 'a.spec.mjs', specs: [{title: 'fits', file: 'a.spec.mjs', tests: [{expectedStatus: 'passed', results: [{status: 'failed'}, {status: 'passed'}]}]}]}]}]});

test('a passing run matches the matrix; partial rows may fail', () => {
  const rows = parseMatrix(matrix);
  assert.deepEqual(rows.map(r => [r.feature, r.status, r.tests.length]), [['Create', 'works', 1], ['Phone', 'works', 2], ['Budget', 'partial', 1], ['Account', 'missing', 0]]);
  assert.deepEqual(check(rows, testResults(report('passed'))), []);
});

test('a works row fails when its test fails, is skipped or is gone', () => {
  const rows = parseMatrix(matrix);
  for (const status of ['failed', 'skipped']) assert.match(check(rows, testResults(report(status))).join('\n'), /Create: marked works but its test did not pass/);
  const gone = report('passed');
  gone.suites[0].specs.shift();
  assert.match(check(rows, testResults(gone)).join('\n'), /Create: test not found/);
});

test('a works row needs a test and a version; statuses are fixed', () => {
  const errors = check(parseMatrix('| Feature | Status | Last passed | Test |\n| X | works | — | none |\n| Y | done | v1.0.0 | |'), new Map());
  assert.deepEqual(errors, ['X: marked works but names no test', 'X: marked works but has no last-passed version', 'Y: status must be one of works, partial, missing, not "done"']);
});
