# `info` and `get` on images the oracle made: every canonical key, with the
# right type, and the values `qemu-img info` reports for the same file --
# virtual size, block size (qemu's cluster size), backing file, dirty flag.
# And `info` is only a look: the file is byte-identical afterwards.
source "$(dirname "$0")/lib.sh"
source "$(dirname "$0")/images.sh"

cd "$SANDBOX" || exit 1
make_images

for img in $IMAGES; do
    cp "$img" "$img.before"
    img.vmdk "$img" info >"$img.json"
    check "info on $img exits 0" test $? -eq 0
    same "info left $img as it was" "$img" "$img.before"
    qemu-img info -f vmdk --output=json "$img" >"$img.qemu.json"

    jq_check "$img: the canonical keys, in order" \
        '[keys_unsorted[]] == ["format","virtual_size","block_size","backing","dirty","vmdk"]' "$img.json"
    jq_check "$img: format is vmdk" '.format == "vmdk"' "$img.json"
    jq_check "$img: the sizes are numbers" \
        '[.virtual_size, .block_size, .vmdk.version, .vmdk.grain_table_entries, .vmdk.extents] | all(type == "number")' "$img.json"
    jq_check "$img: the flags are booleans" \
        '[.dirty, .vmdk.zeroed_grain, .vmdk.redundant_grain_directory] | all(type == "boolean")' "$img.json"
    jq_check "$img: backing is null (no child image opens)" '.backing == null' "$img.json"
    jq_check "$img: dirty is false (qemu-img closed it)" '.dirty == false' "$img.json"
    jq_check "$img: one extent, in the file itself" '.vmdk.extents == 1' "$img.json"
    jq_check "$img: 512 grain-table entries, qemu-img's" '.vmdk.grain_table_entries == 512' "$img.json"
    want_type="$(create_type_of "$img")"
    jq_check "$img: create_type is $want_type" --arg t "$want_type" '.vmdk.create_type == $t' "$img.json"

    # The oracle's view of the same file.
    jq_check "$img: virtual_size is qemu-img's" \
        --slurpfile q "$img.qemu.json" '.virtual_size == $q[0]."virtual-size"' "$img.json"
    jq_check "$img: block_size is qemu-img's cluster size" \
        --slurpfile q "$img.qemu.json" '.block_size == $q[0]."cluster-size"' "$img.json"
    jq_check "$img: backing is qemu-img's backing-filename" \
        --slurpfile q "$img.qemu.json" '.backing == $q[0]."backing-filename"' "$img.json"
    jq_check "$img: dirty is qemu-img's dirty-flag" \
        --slurpfile q "$img.qemu.json" '.dirty == $q[0]."dirty-flag"' "$img.json"
    jq_check "$img: create_type is qemu-img's create-type" \
        --slurpfile q "$img.qemu.json" '.vmdk.create_type == $q[0]."format-specific".data."create-type"' "$img.json"
    jq_check "$img: extents is qemu-img's extent count" \
        --slurpfile q "$img.qemu.json" '.vmdk.extents == ($q[0]."format-specific".data.extents | length)' "$img.json"
done

jq_check "sparse.vmdk has 64 KiB grains" '.block_size == 65536' sparse.vmdk.json
jq_check "big.vmdk is 72 MiB" ".virtual_size == $BIG" big.vmdk.json
jq_check "sparse.vmdk is not compressed" '.vmdk.compression == "none"' sparse.vmdk.json
jq_check "stream.vmdk is deflate-compressed" '.vmdk.compression == "deflate"' stream.vmdk.json
jq_check "sparse.vmdk has no zeroed-grain marker" '.vmdk.zeroed_grain == false' sparse.vmdk.json
jq_check "zeroed.vmdk has the zeroed-grain marker" '.vmdk.zeroed_grain == true' zeroed.vmdk.json
jq_check "sparse.vmdk has a redundant grain directory" '.vmdk.redundant_grain_directory == true' sparse.vmdk.json

# A writer that did not get to lower uncleanShutdown left the image dirty.
cp sparse.vmdk unclean.vmdk
printf '\001' | dd of=unclean.vmdk bs=1 seek=72 conv=notrunc 2>/dev/null
got="$(img.vmdk unclean.vmdk get dirty --text)"
check "an image with uncleanShutdown set is dirty (got '$got')" test "$got" = "true"

# get KEY answers one key, as an object; --text answers the bare value.
got="$(img.vmdk sparse.vmdk get virtual_size --text)"
check "get virtual_size --text is $SIZE (got '$got')" test "$got" = "$SIZE"
img.vmdk stream.vmdk get vmdk.create_type >key.json
jq_check "get vmdk.create_type is one key" \
    'keys == ["vmdk.create_type"] and .["vmdk.create_type"] == "streamOptimized"' key.json

# info and get are the same verb.
img.vmdk sparse.vmdk get >get.json
img.vmdk sparse.vmdk info >info.json
same "get and info report the same thing" get.json info.json

# --text is key: value lines, nested keys dotted.
img.vmdk sparse.vmdk info --text >info.txt
check "info --text carries format: vmdk" grep -qx 'format: vmdk' info.txt
check "info --text dots the nested keys" grep -qx 'vmdk.create_type: monolithicSparse' info.txt

expect_error "get of an unknown key" 2 img.vmdk sparse.vmdk get no_such_key

finish
