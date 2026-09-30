# A verb the library cannot do still exists and says so: exit status 3, a
# structured error beginning `not implemented`, and the image untouched.
# And a layout the library does not read -- flat, split -- is `not
# implemented` too, naming the create type, rather than `not a VMDK`.
source "$(dirname "$0")/lib.sh"

cd "$SANDBOX" || exit 1
qemu-img create -q -f vmdk disk.vmdk 8M
cp disk.vmdk before.vmdk

# not_implemented DESCRIPTION COMMAND...
not_implemented() {
    local what="$1"
    shift
    expect_error "$what" 3 "$@"
    jq_check "$what: the error says not implemented" \
        '.error | startswith("not implemented")' "$SANDBOX/error.json"
}

not_implemented "resize" img.vmdk disk.vmdk resize 16M
not_implemented "set" img.vmdk disk.vmdk set backing base.vmdk
not_implemented "write" img.vmdk disk.vmdk write --offset 0 </dev/null
not_implemented "create" img.vmdk new.vmdk create 8M
same "no refused verb changed the image" disk.vmdk before.vmdk
check "the refused create made no file" test ! -e new.vmdk

# The layouts qemu-img makes that this library refuses at open. Each file
# a user is handed is a text descriptor naming its extents.
for layout in monolithicFlat twoGbMaxExtentSparse twoGbMaxExtentFlat; do
    mkdir "$layout"
    qemu-img create -q -f vmdk -o subformat="$layout" "$layout/disk.vmdk" 8M
    not_implemented "info on a $layout image" img.vmdk "$layout/disk.vmdk" info
    jq_check "the $layout refusal names the create type" \
        --arg t "$layout" '.error | contains($t)' "$SANDBOX/error.json"
    not_implemented "read of a $layout image" img.vmdk "$layout/disk.vmdk" read --length 512
done

# --text turns the error into a line for a person, with the same status.
img.vmdk disk.vmdk resize 16M --text 2>resize.txt
check "resize --text exits 3" test $? -eq 3
check "resize --text says not implemented" grep -q 'img.vmdk: not implemented' resize.txt

finish
