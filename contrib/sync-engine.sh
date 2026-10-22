#!/bin/sh
# Refresh the vendored engine crates from a cosmic-screenshot checkout.
#
#   contrib/sync-engine.sh /path/to/cosmic-screenshot
#
# Copies crates/{ocr,overlay,translation,engine}, prepends the provenance
# header to each src/lib.rs, and records the source commit in crates/VENDORED.md.
set -eu

src=${1:?usage: sync-engine.sh /path/to/cosmic-screenshot}
here=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
commit=$(git -C "$src" rev-parse HEAD)
repo=$(git -C "$src" remote get-url origin 2>/dev/null || echo "unknown")

for crate in ocr overlay translation engine; do
    rm -rf "$here/crates/$crate"
    cp -a "$src/crates/$crate" "$here/crates/$crate"
    rm -rf "$here/crates/$crate/.gitignore"
    lib="$here/crates/$crate/src/lib.rs"
    {
        echo "//! VENDORED — do not edit here."
        echo "//!"
        echo "//! Source: $repo at $commit (crates/$crate)."
        echo "//! Edit there, then run contrib/sync-engine.sh — see crates/VENDORED.md."
        echo "//!"
        cat "$lib"
    } > "$lib.tmp"
    mv "$lib.tmp" "$lib"
done

python3 - "$here/crates/VENDORED.md" "$commit" <<'PY'
import re, sys
path, commit = sys.argv[1], sys.argv[2]
text = open(path).read()
text = re.sub(r"- Commit: `[0-9a-f]+`", f"- Commit: `{commit}`", text)
open(path, "w").write(text)
PY

echo "synced crates/{ocr,overlay,translation,engine} from $commit"
