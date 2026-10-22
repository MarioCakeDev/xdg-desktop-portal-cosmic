# Translate button (fork addition)

This fork adds one button to the screenshot dialog: **Translate**, next to
Capture and the save destination, with a target-language picker beside it.
Pressing it OCRs the selected area, auto-detects the source language, translates
locally (NLLB-200 sidecar) and repaints the captured image **in place**. The
dialog's existing Save/Copy then export the translated image, because the
captured buffer itself was replaced.

There is no second window.

## Base

- Fork of <https://github.com/pop-os/xdg-desktop-portal-cosmic>
- Based on upstream commit `f448ddfb296bf7bb23517b24b2f532559a334a1c`
  (`Epoch 1.11.0 version update`, the code in package
  `1.11.0~1792525517~24.04~f448ddf`).
- Everything else in the portal (ScreenCast, RemoteDesktop, Settings, Access,
  FileChooser) is untouched.

## What the fork touches

| Path | Change |
|---|---|
| `src/screenshot.rs` | dialog state (`Args`), messages, and the background translation task |
| `src/screenshot_translate.rs` | **new**: region geometry, engine call, compositing |
| `src/widget/screenshot.rs` | Translate button, language picker, one-line status row |
| `src/main.rs` | one startup log line identifying the build |
| `Cargo.toml` | workspace members + four path dependencies |
| `Cargo.lock` | regenerated |
| `i18n/en/xdg_desktop_portal_cosmic.ftl` | `translate`, `translating` |
| `crates/{ocr,overlay,translation,engine}` | **new**: vendored engine (see `crates/VENDORED.md`) |
| `contrib/sync-engine.sh` | **new**: refresh the vendored crates |
| `docs/TRANSLATE.md` | this file |

No other source file is modified.

## Runtime dependency

The translation engine is the local NLLB-200 sidecar already installed on
maripop: `127.0.0.1:8765`, `NLLB-200-distilled-600M`, beam 4, fastText for
detection. If it is stopped, the dialog shows one line
(`translation engine not available: …`) under the toolbar and stays open; the
capture is never modified and never half-painted. Nothing blocks the screenshot
flow: OCR and the HTTP call run on a blocking thread, not the UI thread.

## Build

```sh
cargo build --release
cargo test --release            # engine + geometry + compositing tests
```

The binary is `target/release/xdg-desktop-portal-cosmic`.

## Install (gated: replaces a system service)

```sh
# 1. keep the stock binary next to the path, under dpkg's control
sudo dpkg-divert --local --rename --add /usr/libexec/xdg-desktop-portal-cosmic

# 2. install the fork
sudo install -m 0755 target/release/xdg-desktop-portal-cosmic \
  /usr/libexec/xdg-desktop-portal-cosmic

# 3. restart the backend, then the frontend that talks to it
systemctl --user restart org.freedesktop.impl.portal.desktop.cosmic.service
systemctl --user restart xdg-desktop-portal.service
```

Verify which binary is running before trusting the button:

```sh
journalctl --user -u org.freedesktop.impl.portal.desktop.cosmic.service -n 50 \
  | grep "TRANSLATE BUILD"
# xdg-desktop-portal-cosmic TRANSLATE BUILD 1.11.0 — screenshot dialog with in-place Translate
```

## Rollback

```sh
sudo rm -f /usr/libexec/xdg-desktop-portal-cosmic
sudo dpkg-divert --local --rename --remove /usr/libexec/xdg-desktop-portal-cosmic
systemctl --user restart org.freedesktop.impl.portal.desktop.cosmic.service
systemctl --user restart xdg-desktop-portal.service
```

The package file is restored from `/usr/libexec/xdg-desktop-portal-cosmic.distrib`
by `dpkg-divert --remove`.

## Limits

- **Area and screen mode only.** Window picker mode has no persistent selected
  area, so the Translate button is disabled there.
- A rectangle spanning two outputs is translated per output: each dialog's
  toolbar translates the part of the selection on its own screen. Selecting a
  region on one screen is the normal case.
- Untranslated text stays in the original language (no OCR text → no boxes).
- The status line is English, like the engine's one-line errors; the button and
  picker labels come from the i18n catalogue.
