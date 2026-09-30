# Damaged images fail with a structured error and nothing on stdout -- a
# bad magic, a grain-table entry pointing past the end of the file, a
# truncated file -- and the oracle
# agrees each one is damaged: `qemu-img check` reports it, or refuses to
# open it. No damaged image is changed by being looked at.
#
# NOT HERE: a grain-directory entry past the end of the file. This library
# refuses it; qemu-img reads the missing table as zeros and its check calls
# the image clean (measured 2026-09-30, with the entry damaged in both
# copies of the directory). With no oracle agreeing it is damage, it is
# left to the crate's own corruption tests.
source "$(dirname "$0")/lib.sh"

cd "$SANDBOX" || exit 1

# poke FILE OFFSET BYTE: overwrite one byte.
poke() {
    printf "\\$(printf '%03o' "$3")" | dd of="$1" bs=1 seek="$2" conv=notrunc 2>/dev/null
}

# le FILE OFFSET COUNT: COUNT bytes at OFFSET as one little-endian number.
le() {
    local hex
    hex="$(od -An -tx1 -j "$2" -N "$3" "$1" | tr -d ' \n' | sed 's/../& /g' | awk '{for (i = NF; i > 0; i--) printf "%s", $i}')"
    echo $((16#$hex))
}

# put32 FILE OFFSET VALUE: VALUE as a little-endian u32 at OFFSET.
put32() {
    local v="$3"
    printf "\\$(printf '%03o' $((v & 255)))\\$(printf '%03o' $(((v >> 8) & 255)))\\$(printf '%03o' $(((v >> 16) & 255)))\\$(printf '%03o' $(((v >> 24) & 255)))" |
        dd of="$1" bs=1 seek="$2" conv=notrunc 2>/dev/null
}

# qemu_rejects DESCRIPTION IMAGE: `qemu-img check` exits non-zero on it.
qemu_rejects() {
    if qemu-img check -f vmdk "$2" >"$SANDBOX/check.out" 2>&1; then
        fail "$1: qemu-img check calls it clean: $(head -c 300 "$SANDBOX/check.out")"
    else
        ok
    fi
}

# damaged DESCRIPTION IMAGE VERB...: the verb fails as a structured error,
# and the image is as it was.
damaged() {
    local what="$1" img="$2"
    shift 2
    cp "$img" "$img.before"
    expect_error "$what" 1 img.vmdk "$img" "$@"
    same "$what: the image is as it was" "$img" "$img.before"
}

qemu-img create -q -f vmdk -o subformat=monolithicSparse good.vmdk 8M
qemu-io -f vmdk -c "write -P 0x42 0 65536" good.vmdk >/dev/null
check "qemu-img check calls the undamaged image clean" qemu-img check -q -f vmdk good.vmdk

# The magic is not KDMV.
cp good.vmdk bad-magic.vmdk
poke bad-magic.vmdk 0 0
damaged "a bad magic" bad-magic.vmdk info
damaged "a bad magic, read" bad-magic.vmdk read
qemu_rejects "a bad magic" bad-magic.vmdk

# The grain directory (gdOffset, in sectors, at byte 56) and the first
# grain table it names.
gd=$(($(le good.vmdk 56 8) * 512))
gt=$(($(le good.vmdk "$gd" 4) * 512))
check "the grain directory and its first table are found (at $gd, $gt)" test "$gd" -gt 0 -a "$gt" -gt 0

# The first grain-table entry, the grain qemu-io wrote, points far past the
# end of the file.
cp good.vmdk gte-past-end.vmdk
put32 gte-past-end.vmdk "$gt" $((0x10000000))
damaged "a grain-table entry past the end of the file" gte-past-end.vmdk read --length 512
qemu_rejects "a grain-table entry past the end of the file" gte-past-end.vmdk

# Cut short: the header survives, the tables and data do not.
head -c 1024 good.vmdk >truncated.vmdk
damaged "a truncated image" truncated.vmdk read
qemu_rejects "a truncated image" truncated.vmdk

# Not an image at all, and an empty file.
head -c 65536 /dev/urandom >noise.bin
expect_error "a file of noise" 1 img.vmdk noise.bin info
: >empty.vmdk
expect_error "an empty file" 1 img.vmdk empty.vmdk info
expect_error "a file that is not there" 1 img.vmdk missing.vmdk info

finish
