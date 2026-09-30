# tests/cli/images.sh -- the images the suite reads, every one made by the
# oracle: this library has no creator, and an image of our own making would
# check our reader against our writer's reading of the format.
#
# make_images builds, in the working directory, and lists in $IMAGES:
#
#   sparse.vmdk      monolithicSparse, qemu-img's default, with patterns in
#                    the first grain, a range written as zeros (`write -z`),
#                    ranges never written, and the last sector
#   zeroed.vmdk      the same writes with `zeroed_grain=on`, so the zeroed
#                    range is recorded as zeroed-grain markers, not data
#   big.vmdk         monolithicSparse over 72 MiB: three grain tables, the
#                    last only partly inside the disk
#   converted.vmdk   a raw image converted by qemu-img, which leaves the
#                    all-zero grains unallocated
#   stream.vmdk      streamOptimized, converted from the same raw image:
#                    every grain deflate-compressed
#
# Flat, split and VMFS layouts are not here: the library refuses them at
# open, which tests/cli/test-unsupported.sh checks.
#
# Sourced by the test files that need them; lib.sh has already been.

MiB=1048576
SIZE=$((8 * MiB))
BIG=$((72 * MiB))

# fill SIZE IMAGE: the shared pattern of writes, the last one at the end of
# a disk of SIZE bytes.
fill() {
    local size="$1" img="$2"
    qemu-io -f vmdk \
        -c "write -P 0x5a 4096 8192" \
        -c "write -P 0xc3 $((1 * MiB - 1000)) 3000" \
        -c "write -P 0x3c $((3 * MiB)) $((256 * 1024))" \
        -c "write -z $((3 * MiB + 64 * 1024)) $((64 * 1024))" \
        -c "write -P 0x7e $((size - 512)) 512" \
        "$img" >/dev/null
}

make_images() {
    qemu-img create -q -f vmdk -o subformat=monolithicSparse sparse.vmdk "$SIZE"
    fill "$SIZE" sparse.vmdk
    qemu-img create -q -f vmdk -o subformat=monolithicSparse,zeroed_grain=on zeroed.vmdk "$SIZE"
    fill "$SIZE" zeroed.vmdk
    qemu-img create -q -f vmdk -o subformat=monolithicSparse big.vmdk "$BIG"
    fill "$BIG" big.vmdk
    qemu-io -f vmdk -c "write -P 0x11 $((40 * MiB - 100)) 200" big.vmdk >/dev/null
    qemu-img convert -f vmdk -O raw sparse.vmdk sparse.raw
    qemu-img convert -f raw -O vmdk -o subformat=monolithicSparse sparse.raw converted.vmdk
    qemu-img convert -f raw -O vmdk -o subformat=streamOptimized sparse.raw stream.vmdk
    rm -f sparse.raw
    IMAGES="sparse.vmdk zeroed.vmdk big.vmdk converted.vmdk stream.vmdk"
}

# size_of IMAGE: its virtual size, as made above.
size_of() {
    case "$1" in
        big.vmdk) echo "$BIG" ;;
        *) echo "$SIZE" ;;
    esac
}

# create_type_of IMAGE: the createType its descriptor records.
create_type_of() {
    case "$1" in
        stream.vmdk) echo streamOptimized ;;
        *) echo monolithicSparse ;;
    esac
}
