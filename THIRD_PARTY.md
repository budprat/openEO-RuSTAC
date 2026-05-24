# Third-Party Attributions

This file records non-trivial ports or direct translations from
permissively-licensed third-party source code. Each entry includes the
upstream project, version/commit, licence, and the orbit module(s) that
adapt it.

## Format

```
### <project-name>
- **Upstream**: <URL>
- **Version / commit**: <semver or git sha>
- **Licence**: <SPDX ID, e.g. Apache-2.0>
- **Used in**: crates/<crate>/src/<file>.rs
- **Adaptation summary**: <what was ported and what changed>
```

---

## Entries

*(none yet — placeholder for future Week 6+ ports such as sentinel-hub/
sentinel2-cloud-detector for s2cloudless)*

---

## Things explicitly NOT in this file

- **Cargo dependencies**: see `Cargo.lock` + each crate's `Cargo.toml`. The
  licences of transitive dependencies are tracked by `cargo deny` (see
  `deny.toml` once added in Week 1.6).
- **Design inspiration only** (no code): see `NOTICE.md` for upstream
  attribution. Design inspiration only; no code from upstream is included.
- **Specs / standards / papers**: see `NOTICE.md`. Citations of standards
  and papers don't require entries here.
