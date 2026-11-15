#!/usr/bin/env node
// Single source of truth for the platform version.
//
// `VERSION` at the repository root is authoritative. Every other copy is a
// generated value: `set` writes them, `check` verifies them, `assert` proves a
// release tag was cut from the version the tree actually declares.
//
//   node dev/common/version.mjs check
//   node dev/common/version.mjs set 0.2.0
//   node dev/common/version.mjs assert v0.2.0
//   node dev/common/version.mjs current
//
// No third-party dependencies: this runs on a bare CI runner before npm ci.

import { readFileSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '../..');
const VERSION_FILE = join(ROOT, 'VERSION');
const SEMVER = /^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$/;

// Each source owns exactly one version literal. Patterns are anchored to the
// declaration they belong to, so dependency version keys are never touched.
// Groups: (1) prefix, (2) version, (3) suffix.
const SOURCES = [
    {
        file: 'core/Cargo.toml',
        re: /^(\[workspace\.package\][\s\S]*?^version[ \t]*=[ \t]*")([^"]+)(")/m,
    },
    {
        file: 'apps/acowork-desktop/package.json',
        re: /^([ \t]*"version"[ \t]*:[ \t]*")([^"]+)(")/m,
    },
    {
        file: 'apps/acowork-desktop/src-tauri/tauri.conf.json',
        re: /^([ \t]*"version"[ \t]*:[ \t]*")([^"]+)(")/m,
    },
    {
        file: 'apps/acowork-desktop/src-tauri/Cargo.toml',
        re: /^(version[ \t]*=[ \t]*")([^"]+)(")/m,
    },
    {
        file: 'apps/acowork-mobile/package.json',
        re: /^([ \t]*"version"[ \t]*:[ \t]*")([^"]+)(")/m,
    },
    {
        file: 'apps/acowork-mobile/src-tauri/tauri.conf.json',
        re: /^([ \t]*"version"[ \t]*:[ \t]*")([^"]+)(")/m,
    },
    {
        file: 'apps/acowork-mobile/src-tauri/Cargo.toml',
        re: /^(version[ \t]*=[ \t]*")([^"]+)(")/m,
    },
];

function readSource(entry) {
    const text = readFileSync(join(ROOT, entry.file), 'utf8');
    const match = text.match(entry.re);
    if (!match) {
        throw new Error(`cannot locate the version literal in ${entry.file}`);
    }
    return { text, current: match[2] };
}

function canonical() {
    return readFileSync(VERSION_FILE, 'utf8').trim();
}

function assertSemver(version) {
    if (!SEMVER.test(version)) {
        throw new Error(`not a semver version: "${version}" (expected MAJOR.MINOR.PATCH[-prerelease][+build])`);
    }
}

function check() {
    const expected = canonical();
    assertSemver(expected);
    const failures = [];
    for (const entry of SOURCES) {
        let current;
        try {
            current = readSource(entry).current;
        } catch (error) {
            failures.push(`${entry.file}: ${error.message}`);
            continue;
        }
        if (current !== expected) {
            failures.push(`${entry.file}: ${current} (expected ${expected})`);
        }
    }
    if (failures.length > 0) {
        console.error(`Version consistency FAILED against VERSION (${expected}):`);
        for (const failure of failures) console.error(`  - ${failure}`);
        console.error('\nFix: node dev/common/version.mjs set ' + expected);
        process.exit(1);
    }
    console.log(`Version consistency OK: ${expected} across VERSION + ${SOURCES.length} declarations`);
}

function set(version) {
    assertSemver(version);
    // Read every source before writing anything, so a broken pattern cannot
    // leave the tree half-updated.
    const snapshots = SOURCES.map((entry) => ({ entry, ...readSource(entry) }));
    for (const { entry, text } of snapshots) {
        const next = text.replace(entry.re, (_all, prefix, _old, suffix) => `${prefix}${version}${suffix}`);
        writeFileSync(join(ROOT, entry.file), next);
    }
    writeFileSync(VERSION_FILE, `${version}\n`);
    console.log(`Version set to ${version} in VERSION + ${SOURCES.length} declarations.`);
    console.log('Review the diff, rebuild changed .agent packages, then commit together.');
}

function assertTag(tag) {
    const expected = canonical();
    const actual = tag.replace(/^v/, '');
    if (actual !== expected) {
        console.error(`Tag ${tag} does not match VERSION ${expected}.`);
        console.error('Cut the tag from a commit whose VERSION was bumped, or fix the tag name.');
        process.exit(1);
    }
    console.log(`Tag ${tag} matches VERSION ${expected}.`);
}

const [command, argument] = process.argv.slice(2);
try {
    switch (command) {
        case 'check':
            check();
            break;
        case 'set':
            if (!argument) throw new Error('set requires a version, e.g. set 0.2.0');
            set(argument);
            break;
        case 'assert':
            if (!argument) throw new Error('assert requires a tag, e.g. assert v0.2.0');
            assertTag(argument);
            break;
        case 'current':
            console.log(canonical());
            break;
        default:
            console.log('Usage: node dev/common/version.mjs <check|set <version>|assert <tag>|current>');
            if (command && command !== '--help' && command !== '-h') process.exit(1);
    }
} catch (error) {
    console.error(`version.mjs: ${error.message}`);
    process.exit(1);
}
