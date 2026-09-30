# `write --offset` puts exactly the bytes on stdin at exactly that offset,
# into every kind of grain -- allocated, never written, marked zeroed, in
# the last grain table -- and the oracle agrees:
# after each image's writes `qemu-img check` finds no errors, and
# `qemu-img convert -O raw` is the image as it was with those bytes laid
# over it by dd, and nothing else changed. A streamOptimized image is
# refused, and left as it was.
source "$(dirname "$0")/lib.sh"
source "$(dirname "$0")/images.sh"

cd "$SANDBOX" || exit 1
make_images

# overlay FILE OFFSET INPUT: INPUT's bytes over FILE at OFFSET.
overlay() {
    dd if="$3" of="$1" bs=1 seek="$2" conv=notrunc 2>/dev/null
}

random() {
    head -c "$2" /dev/urandom >"$1"
}

# The writes, as OFFSET LENGTH: inside the first written run (an allocated
# grain), across a 64 KiB grain boundary, inside the range written as zeros
# (a zeroed-grain marker in zeroed.vmdk), in the never-written 5 MiB,
# straddling 2 MiB, and the last byte of the disk.
writes() {
    local size="$1"
    echo "4100 700"
    echo "$((1 * MiB - 300)) 5000"
    echo "$((3 * MiB + 64 * 1024 + 17)) 9000"
    echo "$((5 * MiB + 12345)) 70000"
    echo "$((2 * MiB - 4096)) 8192"
    echo "$((size - 1)) 1"
}

for img in sparse.vmdk zeroed.vmdk big.vmdk converted.vmdk; do
    size="$(size_of "$img")"
    qemu-img convert -f vmdk -O raw "$img" "$img.want"
    i=0
    while read -r offset length; do
        i=$((i + 1))
        random "$img.in$i" "$length"
        if [ $((i % 2)) -eq 0 ]; then
            cat "$img.in$i" | img.vmdk "$img" write --offset "$offset" >"$img.w$i.json"
            check "$img: a piped write of $length at $offset exits 0" test "${PIPESTATUS[1]}" -eq 0
        else
            img.vmdk "$img" write --offset "$offset" <"$img.in$i" >"$img.w$i.json"
            check "$img: a write of $length at $offset exits 0" test $? -eq 0
        fi
        jq_check "$img: write reports its offset and count" \
            ".offset == $offset and .bytes == $length" "$img.w$i.json"
        overlay "$img.want" "$offset" "$img.in$i"
        img.vmdk "$img" read --offset "$offset" --length "$length" >"$img.back"
        same "$img: read at $offset returns what was written" "$img.back" "$img.in$i"
    done < <(writes "$size")

    qemu-img check -f vmdk "$img" >"$img.check" 2>&1
    check "$img: qemu-img check finds no errors after our writes: $(tr '\n' ' ' <"$img.check" | head -c 300)" \
        test "${PIPESTATUS[0]}" -eq 0
    qemu-img convert -f vmdk -O raw "$img" "$img.qemu.raw"
    same "$img: qemu-img reads exactly the bytes written, where written, and nothing else changed" \
        "$img.qemu.raw" "$img.want"
    img.vmdk "$img" read >"$img.ours.raw"
    same "$img: our whole-disk read agrees with qemu-img" "$img.ours.raw" "$img.want"
    img.vmdk "$img" info >"$img.after.json"
    jq_check "$img: our writer closed the image: no log left to replay" '.dirty == false' "$img.after.json"
done

# Into a never-written grain of big.vmdk's third grain table, the one only
# partly inside the disk.
random last.bin 4096
qemu-img convert -f vmdk -O raw big.vmdk big.want
img.vmdk big.vmdk write --offset $((70 * MiB)) <last.bin >/dev/null
check "a write into the last grain table exits 0" test $? -eq 0
overlay big.want $((70 * MiB)) last.bin
check "qemu-img check finds no errors after the last-table write" qemu-img check -q -f vmdk big.vmdk
qemu-img convert -f vmdk -O raw big.vmdk big.qemu.raw
same "qemu-img reads the last-table write" big.qemu.raw big.want

# A streamOptimized image is read, not written: its grains are compressed
# and append-only. The refusal names why, and the image is as it was.
cp stream.vmdk stream.before
printf 'x' | img.vmdk stream.vmdk write --offset 0 >/dev/null 2>stream.json
check "a streamOptimized image refuses the write with exit 3" test "${PIPESTATUS[1]}" -eq 3
jq_check "the streamOptimized refusal says not implemented, and why" \
    '(.error | startswith("not implemented")) and (.error | test("stream-optimized"))' stream.json
same "the refused streamOptimized image is as it was" stream.vmdk stream.before

# Nothing on stdin writes nothing.
img.vmdk sparse.vmdk write --offset 0 </dev/null >empty.json
jq_check "an empty write reports 0 bytes" '.bytes == 0' empty.json

# Input that would run past the end is refused before anything is written,
# from a file and from a pipe; so is an offset past the end, and the image
# as its own input.
cp sparse.vmdk before.vmdk
random past.bin 1024
expect_error "a file running past the end" 1 img.vmdk sparse.vmdk write --offset $((SIZE - 512)) <past.bin
cat past.bin | img.vmdk sparse.vmdk write --offset $((SIZE - 512)) >/dev/null 2>past.json
check "a pipe running past the end exits 1" test "${PIPESTATUS[1]}" -eq 1
expect_error "an offset past the end" 1 img.vmdk sparse.vmdk write --offset $((SIZE + 1)) </dev/null
expect_error "the image as its own input" 1 img.vmdk sparse.vmdk write --offset 0 <sparse.vmdk
same "no refused write changed the image" sparse.vmdk before.vmdk
expect_error "write without --offset" 2 img.vmdk sparse.vmdk write </dev/null

# A read-only image file is refused, not written behind its mode.
cp sparse.vmdk locked.vmdk
chmod 0444 locked.vmdk
cp locked.vmdk locked.before
printf 'x' | img.vmdk locked.vmdk write --offset 0 >/dev/null 2>locked.json
check "a read-only image file refuses the write with exit 1" test "${PIPESTATUS[1]}" -eq 1
same "the read-only image file is as it was" locked.vmdk locked.before
chmod 0644 locked.vmdk

finish
