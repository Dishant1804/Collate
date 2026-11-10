# Collate logo

Approved Compact C geometry in four finishes: Graphite, Compact, Porcelain and Cobalt.

- `variants/preview.html`: interactive comparison.
- `variants/*.svg`: four finished icon concepts and one shared flat figure.
- `variants/specs.json`: geometry, colours and finish settings.
- `state.json`: decisions and verification history.

Regenerate from the repository root:

```sh
node logo-work/variants/draw.mjs
node logo-work/variants/present.mjs
```

Application assets have not been replaced. Installed Dock testing and production exports remain pending.
