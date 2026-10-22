# Vendored engine crates

These four crates are copied from the cosmic-screenshot fork:

| Crate | Source path |
|---|---|
| `cosmic-screenshot-ocr` | `crates/ocr` |
| `cosmic-screenshot-overlay` | `crates/overlay` |
| `cosmic-screenshot-translation` | `crates/translation` |
| `cosmic-screenshot-engine` | `crates/engine` |

- Repository: <https://github.com/MarioCakeDev/cosmic-screenshot>
- Commit: `0da621db9166852fe5deffaefa2f646535538141`
  — binarised screenshot OCR (probe → narrow, one psm 6 pass, psm 11 only for
  a short reading), the honest found/kept/translated/drawn counts and the
  `COSMIC_SCREENSHOT_DEBUG_DIR` dump.

Each `src/lib.rs` carries a five-line provenance header naming that commit.
Apart from the header the files are byte-identical to the source.

## Why vendored, not a git dependency

The portal packages with `cargo vendor` + `--frozen` (`just vendor`, see
`debian/rules`). Path dependencies inside the source tree stay offline-buildable
and are part of the reviewed patch, and the portal remains a single `git clone`
plus `cargo build --release`. A second repository would add a fetch that the
Debian build cannot vendor, for no benefit at this size.

## Why one shared crate instead of portal-local code

The portal must not grow its own OCR, colour sampling or text-fitting code: two
implementations drift, and the known "translated label sits over the wrong
glyphs" class of bug comes straight back. `crates/engine` is the only copy of
the two-pass OCR decision (`für` → `fur`), the language table, and the
translate-and-paint call. The CLI (`cosmic-screenshot-translate`) and the
screenshot dialog both call `engine::translate_canvas`.

## Refreshing

```sh
./contrib/sync-engine.sh /path/to/cosmic-screenshot
```

The script lives in this repository (`contrib/sync-engine.sh`). It copies the
four crates, rewrites the headers, and updates this file with the new commit.
Do not edit the vendored crates here; change them in the source repository
first.
