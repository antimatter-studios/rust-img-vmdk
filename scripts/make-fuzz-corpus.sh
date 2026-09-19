#!/usr/bin/env bash
# Rebuild fuzz/corpus from images qemu-img wrote.
#
# VMDK is two parsers, not one, and they fail differently. The binary
# sparse header carries grain_size, num_gtes_per_gt, gd_offset and
# capacity, all used in arithmetic on the read path -- a grain size that
# multiplied out to zero and divided by zero on the first read was a
# real finding in this family on 2026-09-06. The descriptor is TEXT:
# unbounded line lengths, extent lines with counts that need not match
# the file, and parent links.
#
# Usage: scripts/make-fuzz-corpus.sh
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
work="$(mktemp -d "${TMPDIR:-/tmp}/vmdk-fuzz-corpus.XXXXXX")"
trap 'rm -rf "$work"' EXIT

command -v qemu-img >/dev/null || {
    echo "qemu-img not found; install qemu-utils" >&2
    exit 1
}

payload="$work/payload"
python3 - "$payload" <<'PY'
import sys
with open(sys.argv[1], 'wb') as f:
    f.write(b'A' * 8_000)
    f.seek(500_000)
    f.write(b'B' * 8_000)
PY

rm -rf "$here/fuzz/corpus"
mkdir -p "$here/fuzz/corpus"/{image,header,descriptor}

build() {
    local name="$1"; shift
    qemu-img convert -f raw -O vmdk "$@" "$payload" "$here/fuzz/corpus/image/$name.vmdk" \
        2>/dev/null || {
        echo "qemu-img could not build the '$name' image" >&2
        exit 1
    }
}

# The subformats are different files, not flags. A sparse image has an
# embedded descriptor behind its header; a stream-optimized one is
# compressed and laid out for sequential reading; a flat one is a
# descriptor pointing at a separate raw extent.
build sparse           -o subformat=monolithicSparse
build stream_optimized -o subformat=streamOptimized

python3 - "$here/fuzz/corpus" <<'PY'
import os, struct, sys

root = sys.argv[1]
MAGIC = b'KDMV'
SECTOR = 512

for img_name in sorted(os.listdir(os.path.join(root, 'image'))):
    stem = img_name[:-len('.vmdk')]
    img = open(os.path.join(root, 'image', img_name), 'rb').read()
    assert img[:4] == MAGIC, f"{img_name}: not a sparse VMDK"

    # The sparse header is one sector, and everything the grain
    # directory walk multiplies by is in it.
    with open(os.path.join(root, 'header', f'{stem}.bin'), 'wb') as f:
        f.write(img[:SECTOR])

    # The embedded descriptor: its offset and size are declared in the
    # header, in sectors, and it is plain text.
    desc_offset, = struct.unpack_from('<Q', img, 0x1c)
    desc_size, = struct.unpack_from('<Q', img, 0x24)
    start = desc_offset * SECTOR
    end = start + desc_size * SECTOR
    assert 0 < start < len(img), f"{img_name}: descriptor offset {desc_offset} is not in the file"
    text = img[start:end]
    assert b'version=' in text, f"{img_name}: no descriptor text at the declared offset"
    # Trimmed at the first NUL: the region is padded to a whole number
    # of sectors and the parser is handed the text, not the padding.
    with open(os.path.join(root, 'descriptor', f'{stem}.txt'), 'wb') as f:
        f.write(text.split(b'\x00', 1)[0])
PY

echo "corpus rebuilt under fuzz/corpus:"
find "$here/fuzz/corpus" -type f | sort | sed "s#$here/##"
echo "total: $(find "$here/fuzz/corpus" -type f | wc -l) seeds, $(du -sh "$here/fuzz/corpus" | cut -f1)"
