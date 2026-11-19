# doc-clone — hosted-app install fixture

Clones the `acowork-doc` binary as a non-builtin hosted application
(`id = doc-clone`) so ADR-116 install / MCP / data-isolation can be verified
without migrating the Desktop DOC UI.

## Pack

```bash
# From repo root; builds an unsigned .app into build/
python dev/common/pack-doc-clone.py
# or: bash dev/common/pack-doc-clone.sh
```

Requires a built `acowork-doc` (debug or release) under `core/target/` or
`target/`, plus `acowork-pack-app` (or `cargo run -p acowork-sign --bin
acowork-pack-app`).

## Install (isolated Gateway)

```bash
python dev/common/smoke-doc-clone.py
```
