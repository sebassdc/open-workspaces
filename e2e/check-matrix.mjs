// Fails when docs/UI_FEATURES.md and the last Playwright run disagree: every row
// marked "works" must name tests, and every named test must exist and have passed.
// With --base OLD.md, a row marked "works" there must still be "works" here.
// Usage: node check-matrix.mjs [--base OLD.md] [matrix.md] [results.json]
import fs from 'node:fs';
import path from 'node:path';
import {fileURLToPath} from 'node:url';

const STATUSES = ['works', 'partial', 'missing'];

// Rows of every table whose second column is a status; tests are `file › title` code spans.
export function parseMatrix(markdown) {
  const rows = [];
  for (const line of markdown.split('\n')) {
    const cells = line.trim().replace(/^\||\|$/g, '').split('|').map(cell => cell.trim());
    if (!line.trim().startsWith('|') || cells.length < 4) continue;
    const [feature, status, version, tests] = cells;
    if (feature === 'Feature' || /^-+$/.test(feature)) continue;
    rows.push({feature, status, version, tests: [...tests.matchAll(/`([^`]+ › [^`]+)`/g)].map(m => m[1])});
  }
  return rows;
}

// "file › describe › title" -> passed?, from Playwright's JSON reporter output.
export function testResults(report) {
  const results = new Map();
  const walk = (suite, titles) => {
    const inner = suite.file && suite.title === suite.file ? titles : [...titles, suite.title];
    for (const spec of suite.specs || []) {
      const passed = spec.tests.length > 0 && spec.tests.every(t => t.expectedStatus === 'passed' && t.results.at(-1)?.status === 'passed');
      const key = [path.basename(spec.file), ...inner.filter(Boolean), spec.title].join(' › ');
      results.set(key, (results.get(key) ?? true) && passed);
    }
    for (const child of suite.suites || []) walk(child, inner);
  };
  for (const suite of report.suites || []) walk(suite, []);
  return results;
}

export function check(rows, results) {
  const errors = [];
  if (!rows.length) errors.push('no feature rows found');
  for (const row of rows) {
    if (!STATUSES.includes(row.status)) errors.push(`${row.feature}: status must be one of ${STATUSES.join(', ')}, not "${row.status}"`);
    if (row.status === 'works' && !row.tests.length) errors.push(`${row.feature}: marked works but names no test`);
    if (row.status === 'works' && !/^v\d+\.\d+\.\d+$/.test(row.version)) errors.push(`${row.feature}: marked works but has no last-passed version`);
    for (const test of row.tests) {
      if (!results.has(test)) errors.push(`${row.feature}: test not found in this run: ${test}`);
      else if (!results.get(test)) errors.push(`${row.feature}: marked ${row.status} but its test did not pass: ${test}`);
    }
  }
  return errors;
}

// A working feature may not quietly lose its row or status.
export function downgrades(base, rows) {
  const now = new Map(rows.map(row => [row.feature, row.status]));
  return base.filter(row => row.status === 'works' && now.get(row.feature) !== 'works')
    .map(row => `${row.feature}: was works, now ${now.get(row.feature) ?? 'removed'}`);
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const here = path.dirname(fileURLToPath(import.meta.url));
  const args = process.argv.slice(2), at = args.indexOf('--base');
  const base = at < 0 ? null : args.splice(at, 2)[1];
  const matrix = args[0] || path.join(here, '../docs/UI_FEATURES.md');
  const report = args[1] || path.join(here, 'playwright-report/results.json');
  const rows = parseMatrix(fs.readFileSync(matrix, 'utf8'));
  const results = testResults(JSON.parse(fs.readFileSync(report, 'utf8')));
  const errors = check(rows, results);
  if (base) errors.push(...downgrades(parseMatrix(fs.readFileSync(base, 'utf8')), rows));
  const named = new Set(rows.flatMap(row => row.tests));
  for (const test of results.keys()) if (!named.has(test)) console.warn(`note: no matrix row names ${test}`);
  for (const error of errors) console.error(`UI feature matrix: ${error}`);
  if (errors.length) process.exit(1);
  console.log(`UI feature matrix: ${rows.length} rows match ${results.size} tests`);
}
