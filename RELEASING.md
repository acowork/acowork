# Releasing ACowork

The release contract for maintainers and contributors. Read
[`CONTRIBUTING.md`](CONTRIBUTING.md) first — every merge gate there also
applies to a release commit. Credential procurement and CI secret setup are
maintainer-only operations and are intentionally not part of this document.

**Principles**

- `github.com/acowork/ACowork` is the review and release authority. The Gitee
  remote is a mirror, synced **after** the GitHub Release exists.
- One thing triggers a release: an annotated `v*` tag. Never move or delete a
  published tag — cut a patch release.
- The workflow publishes a **draft**. A maintainer verifies the assets and
  presses Publish; nothing reaches users automatically.
- The tag must point at a `master` commit whose CI is green.
- **The first release is `v0.1.0`.**

---

## 1. Version sources

[`VERSION`](VERSION) is authoritative.
[`dev/common/version.mjs`](dev/common/version.mjs) owns the other seven
declarations: `core/Cargo.toml` plus desktop/mobile `package.json`,
`tauri.conf.json` and `Cargo.toml`.

```bash
node dev/common/version.mjs check          # all declarations agree
node dev/common/version.mjs set 0.1.1       # bump VERSION + 7 declarations
node dev/common/version.mjs assert v0.1.0   # prove a tag matches VERSION
```

`./dev/ci.sh check` and `./dev/ci.sh all` run `check`, so a PR that bumps one
file and forgets the others fails CI.

**Agent packages are versioned independently.** `agents/*/manifest.toml` has
its own `version` and a `runtime_version` compatibility floor. Bump `version`
only for agents you changed, in the same commit as the source edit plus the
rebuilt `.bot`.

---

## 2. Agent packages and the hub mirror

The desktop installer embeds `agents/agent-packages/` as a Tauri resource, so
packages must be rebuilt **before** installers are produced. `build-agent.sh`
pins `SOURCE_DATE_EPOCH` to the last commit that touched the agent directory,
so a release build must come from a **clean, committed** work tree.

```bash
./dev/common/build-agent.sh agents/<name>-agent
cargo run --release --bin acowork-verify -- agents/agent-packages/<id>.bot
```

CI's `packages` job proves the shipped `.bot` matches its source tree. It
only soft-warns about mirror drift; the hard gate lives in the hub's own
publish workflow. One agent change is **two commits, in this order**:

1. Here — edit the agent, bump `version`, rebuild `.bot`, commit.
2. The hub — `dev/common/sync-hub-mirror.sh`, then merge the generated PR.

### Signing baseline

An upgrade is rejected unless the new `.bot`'s signing certificate
fingerprint matches the installed one (`CONTRIBUTING.md` §Signing rules).
`v0.1.0` fixes that baseline: all seven committed packages are signed by

`ceda819f36375e755976fcce1f45c85ba6393a36d3e4ffdd9b16688347501463`

Keep the key that signed them. It lives in the git-ignored
`agents/.signing-keys/`, staged by the maintainer; `build-agent.*` silently
generates a **new** key when `developer.key` is missing, so a fresh clone must
never be the release checkout. Changing this fingerprint later is a documented
uninstall/reinstall migration, not a re-sign.

---

## 3. Release procedure

Run from a clean `master` checkout. Every command is a gate — stop on the first
failure. `VERSION` already says `0.1.0`, so the first release needs no bump.

```bash
git checkout master && git pull github master
git status --porcelain                         # must be empty
node dev/common/version.mjs check
node dev/common/version.mjs assert v0.1.0
./dev/ci.sh all                                # stricter than CI: + frontend smoke
git add -A && git commit -m "chore(release): prepare v0.1.0"
#    Open PR → CI green → maintainer merges into master
git checkout master && git pull github master
git tag -a v0.1.0 -m "ACowork v0.1.0"
git push github v0.1.0
```

First release: no previous tag, so release notes fall back to "Initial
release"; validate a **fresh install** only; move
[`CHANGELOG.md`](CHANGELOG.md)'s `[Unreleased]` section to `[0.1.0]`. Later
releases insert `version.mjs set <new>` and add the previous-release upgrade
test.

Pushing the tag starts
[`.github/workflows/release.yml`](.github/workflows/release.yml):

| Job | What it does |
|---|---|
| `verify` | Tag matches `VERSION`, declarations agree, 14 architecture red lines. |
| `build` | Windows / macOS / Ubuntu: nine core binaries → `tauri build` → installers. |
| `publish` | `SHA256SUMS.txt`, Conventional-Commit release notes, **draft** Release. |

Then: verify the asset list and one SHA256, install fresh, press **Publish**,
mirror the tag to Gitee (`git push origin v0.1.0`), and confirm the hub mirror
PR merged if agents changed.

---

## 4. Hotfix, rollback, and known gaps

- Never delete or move a published tag. Fix on `master`, bump the patch, cut a
  new tag. A broken draft can be discarded and rebuilt on the same tag.
- Gitee sync, hub mirror merge, Android APK and iOS submission are manual.
- The Tauri updater is not configured, so v0.1.0 → v0.2.0 is a manual install.
- Windows installer signing needs a decision before the first signed release.
  OS-level installer signing and `.bot` package signing are independent;
  v0.1.0 can ship unsigned installers without affecting package upgrades.
