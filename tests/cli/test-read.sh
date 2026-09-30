# `read` on images the oracle made: the whole disk is byte-identical to
# `qemu-img convert -O raw` for every image, and every range is the same
# slice of that raw image: straddling a grain boundary, inside a zeroed
# range, inside grains never written, the last byte. No read changes the
# image file.
source "$(dirname "$0")/lib.sh"
source "$(dirname "$0")/images.sh"

cd "$SANDBOX" || exit 1
make_images

# slice FILE OFFSET LENGTH: those bytes of FILE, on stdout. GNU tail
# reports the pipe head closes once it has enough; that is not an error.
slice() {
    tail -c +$(($2 + 1)) "$1" 2>/dev/null | head -c "$3"
}

for img in $IMAGES; do
    size="$(size_of "$img")"
    cp "$img" "$img.before"
    qemu-img convert -f vmdk -O raw "$img" "$img.qemu.raw"
    img.vmdk "$img" read >"$img.ours.raw"
    check "read of the whole $img exits 0" test $? -eq 0
    same "the whole of $img, read, is qemu-img's raw image" "$img.ours.raw" "$img.qemu.raw"

    for range in "4096 8192" "$((1 * MiB - 1000)) 3000" "$((3 * MiB + 60000)) 10000" \
        "$((5 * MiB)) 65536" "$((1 * MiB - 4000)) $((2 * MiB))" \
        "$((size - 1)) 1" "$((size - 512)) 512"; do
        set -- $range
        img.vmdk "$img" read --offset "$1" --length "$2" >"$img.range"
        slice "$img.qemu.raw" "$1" "$2" >"$img.want"
        same "$img: read --offset $1 --length $2" "$img.range" "$img.want"
    done
    same "no read changed $img" "$img" "$img.before"
done

# Across big.vmdk's grain-table boundaries (one table covers 32 MiB), and
# into its partly-inside last table.
for range in "$((32 * MiB - 700)) 1400" "$((40 * MiB - 100)) 200" "$((64 * MiB - 10)) $((8 * MiB + 10))"; do
    set -- $range
    img.vmdk big.vmdk read --offset "$1" --length "$2" >big.range
    slice big.vmdk.qemu.raw "$1" "$2" >big.want
    same "big.vmdk: read --offset $1 --length $2" big.range big.want
done

img.vmdk stream.vmdk read -o stream.o.raw
same "read -o of a streamOptimized image writes the same bytes as read to stdout" stream.o.raw stream.vmdk.qemu.raw
check "read -o leaves no .partial file" test ! -e stream.o.raw.partial

# --offset alone reads to the end; --length alone reads from 0.
img.vmdk sparse.vmdk read --offset $((SIZE - 512)) >tail.bin
slice sparse.vmdk.qemu.raw $((SIZE - 512)) 512 >want.bin
same "read --offset alone reads to the end" tail.bin want.bin
img.vmdk sparse.vmdk read --length 16K >head.bin
slice sparse.vmdk.qemu.raw 0 16384 >want.bin
same "read --length alone reads from 0, and takes a suffix" head.bin want.bin

# A range past the end is refused whole, before a byte is written.
expect_error "a range past the end" 1 img.vmdk sparse.vmdk read --offset $((SIZE - 512)) --length 513
expect_error "an offset past the end" 1 img.vmdk sparse.vmdk read --offset $((SIZE + 1))

# An image file nobody may write is read all the same: the read-only verbs
# never open the file for writing.
cp sparse.vmdk locked.vmdk
chmod 0444 locked.vmdk
img.vmdk locked.vmdk read >locked.raw
check "a read-only image file reads" test $? -eq 0
same "a read-only image file reads the same bytes" locked.raw sparse.vmdk.qemu.raw
chmod 0644 locked.vmdk

# A closed pipe is the reader's choice, not a failure.
img.vmdk sparse.vmdk read 2>pipe.err | head -c 100 >/dev/null
check "read into a pipe closed early exits 0" test "${PIPESTATUS[0]}" -eq 0
check "read into a pipe closed early says nothing on stderr" test ! -s pipe.err

finish
