# friendly-snippets (vendored)

Source: https://github.com/rafamadriz/friendly-snippets
Commit: `6cd7280adead7f586db6fccbd15d2cac7e2188b9` (2026-01-23)
License: MIT — see [LICENSE](./LICENSE).

These JSON files are loaded at runtime by rx-helix's local snippet completion
provider. `package.json` maps each language name to its snippet files; the
provider merges every matching file plus a small set of built-in extras
(markdown callouts and the 6x/7x table grid, and the C++ `gfor` loop).

Updating: replace this directory with the upstream `snippets/` directory,
`package.json`, and `LICENSE`, then update the commit above.
