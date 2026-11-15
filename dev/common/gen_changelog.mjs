#!/usr/bin/env node
// Generate release notes from Conventional Commits.
//
//   node dev/common/gen_changelog.mjs <previous-tag> <new-tag>
//   node dev/common/gen_changelog.mjs v0.1.0 v0.2.0 > RELEASE_NOTES.md
//   node dev/common/gen_changelog.mjs "" v0.1.0        # first release
//
// Writes Markdown to stdout. No third-party dependencies.

import { execFileSync } from 'node:child_process';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '../..');
const REPO_URL = 'https://github.com/acowork/ACowork';

const GROUPS = [
    { type: 'feat', title: 'Features' },
    { type: 'fix', title: 'Bug fixes' },
    { type: 'perf', title: 'Performance' },
    { type: 'refactor', title: 'Refactoring' },
    { type: 'docs', title: 'Documentation' },
    { type: 'test', title: 'Tests' },
    { type: 'ci', title: 'CI' },
    { type: 'chore', title: 'Chores' },
    { type: 'style', title: 'Style' },
    { type: 'other', title: 'Other' },
];

function git(args) {
    return execFileSync('git', args, { cwd: ROOT, encoding: 'utf8' }).trim();
}

const previousTag = process.argv[2] || '';
const newTag = process.argv[3] || 'HEAD';
const range = previousTag ? `${previousTag}..${newTag}` : newTag;

let log = '';
try {
    log = git(['log', '--no-merges', '--pretty=%h%x09%s', range]);
} catch (error) {
    console.error(`gen_changelog.mjs: cannot read git log for ${range}: ${error.message}`);
    process.exit(1);
}

const breaking = [];
const buckets = new Map(GROUPS.map((group) => [group.type, []]));
const known = new Set(GROUPS.map((group) => group.type));

for (const line of log.split('\n').filter(Boolean)) {
    const [hash, ...rest] = line.split('\t');
    const subject = rest.join('\t');
    const match = subject.match(/^([a-z]+)(?:\(([^)]+)\))?(!)?:[ \t]*(.+)$/);
    if (!match) {
        buckets.get('other').push(`- ${subject} (${hash})`);
        continue;
    }
    const [, type, scope, bang, summary] = match;
    const item = `- ${scope ? `\`${scope}\`: ` : ''}${summary} (${hash})`;
    const bucket = known.has(type) ? type : 'other';
    buckets.get(bucket).push(item);
    if (bang) breaking.push(item);
}

let date;
try {
    date = git(['log', '-1', '--format=%cs', newTag]) || new Date().toISOString().slice(0, 10);
} catch {
    date = new Date().toISOString().slice(0, 10);
}

const out = [];
out.push(`## ${newTag === 'HEAD' ? 'Unreleased' : newTag} (${date})`);
out.push('');
if (breaking.length > 0) {
    out.push('### Breaking changes', '', ...breaking, '');
}
let count = 0;
for (const group of GROUPS) {
    const items = buckets.get(group.type);
    if (items.length === 0) continue;
    count += items.length;
    out.push(`### ${group.title}`, '', ...items, '');
}
if (count === 0) out.push('_No recorded changes._', '');
if (previousTag) {
    out.push(`**Full changelog**: ${REPO_URL}/compare/${previousTag}...${newTag}`, '');
} else {
    out.push(`**Commits**: ${REPO_URL}/commits/${newTag}`, '');
}

process.stdout.write(`${out.join('\n')}\n`);
