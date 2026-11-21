#!/bin/sh
set -eu

case "${1:-}" in
    --version) printf 'fio-3.39-test\n'; exit 0 ;;
    --enghelp=libaio) exit 0 ;;
esac

for argument do
    case "$argument" in
        --name=*) name=${argument#*=} ;;
        --filename=*) file=${argument#*=} ;;
        --write_bw_log=*) bandwidth=${argument#*=} ;;
        --rw=*) operation=${argument#*=} ;;
        --bs=*) block=${argument#*=} ;;
        --offset=*) offset=${argument#*=} ;;
        --size=*) size=${argument#*=} ;;
        --direct=1) direct=1 ;;
        --ioengine=libaio) asynchronous=1 ;;
        --iodepth=*) depth=${argument#*=} ;;
        --verify=0) no_verify=1 ;;
    esac
done

test "$direct:$asynchronous:$no_verify" = "1:1:1"
test "$((offset % 4096)):$((size % 4096)):$((block % 4096))" = "0:0:0"
printf '%s %s %s %s %s\n' "$operation" "$offset" "$size" "$block" "$depth" >> invocations
printf '1000, 8192\n' > "${bandwidth}_bw.1.log"

completed=$size
errors=0
errno=0
case "$name" in
    *-round-000002-write)
        printf '{"jobs":[{"error":28,"write":{"io_bytes":0}}]}\n'
        exit 1
        ;;
    *-round-000003-write)
        printf 'fio: cannot open namespace for writing: Permission denied\n' >&2
        exit 1
        ;;
    *-read)
        printf 'fio: io_u error on file %s: Input/output error: read offset=0, buflen=131072\n' "$file" >&2
        printf 'fio: io_u error on file %s: No data available: read offset=131072, buflen=131072\n' "$file" >&2
        completed=$((size - 262144))
        errors=2
        errno=5
        ;;
esac

printf '{"jobs":[{"error":%s,"total_err":%s,"%s":{"io_bytes":%s}}]}\n' \
    "$errno" "$errors" "$operation" "$completed"
