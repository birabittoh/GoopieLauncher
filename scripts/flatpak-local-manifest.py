#!/usr/bin/env python3
"""Rewrite the Flatpak manifest to build from a locally-built binary.

The committed manifest (packaging/flatpak/xyz.goopie.launcher.yml) fetches the
launcher from a published GitHub release, which is what a from-source
`flatpak-builder` run or a Flathub-style checker expects. CI, however, needs to
package the binary it just built — before any release exists — so this script
swaps the remote `type: file` sources for a single local one.

Usage:
    flatpak-local-manifest.py <in-manifest> <binary-path> <out-manifest>

The binary is referenced by absolute path, so the output manifest can live in a
different directory than the input (its other relative sources are rewritten to
absolute paths against the input's directory for the same reason).
"""

import os
import sys

import yaml

BINARY_DEST = "goopie-launcher"


def main() -> int:
    if len(sys.argv) != 4:
        print(__doc__, file=sys.stderr)
        return 2

    in_path, binary, out_path = sys.argv[1:4]
    base = os.path.dirname(os.path.abspath(in_path))
    binary = os.path.abspath(binary)

    with open(in_path, encoding="utf-8") as fh:
        manifest = yaml.safe_load(fh)

    module = next(
        (m for m in manifest["modules"]
         if isinstance(m, dict) and m["name"] == "goopie-launcher"),
        None,
    )
    if module is None:
        print("::error::goopie-launcher module not found in manifest", file=sys.stderr)
        return 1

    sources = []
    replaced = False
    for source in module["sources"]:
        # The remote binary sources (one per arch) collapse into one local file.
        if source.get("dest-filename") == BINARY_DEST:
            if not replaced:
                sources.append({"type": "file", "path": binary,
                                "dest-filename": BINARY_DEST})
                replaced = True
            continue
        if "path" in source:
            source = dict(source, path=os.path.join(base, source["path"]))
        sources.append(source)

    if not replaced:
        print("::error::no binary source to replace in manifest", file=sys.stderr)
        return 1

    module["sources"] = sources

    # shared-modules is included by relative path; make it absolute too.
    manifest["modules"] = [
        os.path.join(base, m) if isinstance(m, str) else m
        for m in manifest["modules"]
    ]

    with open(out_path, "w", encoding="utf-8") as fh:
        yaml.safe_dump(manifest, fh, sort_keys=False)
    return 0


if __name__ == "__main__":
    sys.exit(main())
