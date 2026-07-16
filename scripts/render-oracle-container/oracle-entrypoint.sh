#!/bin/sh
set -eu

umask 077

fail() {
    printf 'oracle_error:%s\n' "$1" >&2
    exit 70
}

case "${RXLS_RUN_ID:-}" in
    ''|*[!a-z0-9-]*) fail invalid_run_id ;;
esac
case "${RXLS_SOURCE_EXTENSION:-}" in
    .xls|.xlsx|.xlsm|.xlsb|.ods) ;;
    *) fail invalid_source_extension ;;
esac
case "${RXLS_PRINT_MODE:-}" in
    single-page-sheets)
        export_filter='pdf:calc_pdf_Export:{"SinglePageSheets":{"type":"boolean","value":"true"}}'
        single_page_sheets=true
        ;;
    authored)
        export_filter='pdf:calc_pdf_Export'
        single_page_sheets=false
        ;;
    *) fail invalid_print_mode ;;
esac
case "${RXLS_SOURCE_SHA256:-}" in
    *[!0-9a-f]*|'') fail invalid_source_sha256 ;;
esac
test "${#RXLS_SOURCE_SHA256}" -eq 64 || fail invalid_source_sha256
case "${RXLS_SOURCE_BYTES:-}" in
    ''|*[!0-9]*) fail invalid_source_bytes ;;
esac
case "${RXLS_EVIDENCE_MAX_BYTES:-}" in
    ''|*[!0-9]*) fail invalid_evidence_limit ;;
esac
case "${RXLS_LOCK_SHA256:-}" in
    *[!0-9a-f]*|'') fail invalid_lock_sha256 ;;
esac
test "${#RXLS_LOCK_SHA256}" -eq 64 || fail invalid_lock_sha256
case "${RXLS_FONT_PACK_SHA256:-}" in
    *[!0-9a-f]*|'') fail invalid_font_pack_sha256 ;;
esac
test "${#RXLS_FONT_PACK_SHA256}" -eq 64 || fail invalid_font_pack_sha256

runtime="/oracle/runtime/${RXLS_RUN_ID}"
home="${runtime}/home"
profile="${runtime}/profile"
source="/oracle/source/input${RXLS_SOURCE_EXTENSION}"
pdf="/oracle/evidence/oracle.pdf"

mkdir -p \
    "${home}" \
    "${profile}/user" \
    "${runtime}/cache" \
    "${runtime}/config" \
    "${runtime}/data" \
    "${runtime}/tmp"
cp /opt/rxls/profile/registrymodifications.xcu \
    "${profile}/user/registrymodifications.xcu"

export HOME="${home}"
export XDG_CACHE_HOME="${runtime}/cache"
export XDG_CONFIG_HOME="${runtime}/config"
export XDG_DATA_HOME="${runtime}/data"
export TMPDIR="${runtime}/tmp"
export FONTCONFIG_FILE=/oracle/fonts/fonts.conf
export FONTCONFIG_PATH=/oracle/fonts
export SAL_USE_VCLPLUGIN=svp
export SAL_DISABLE_OPENCL=1
export SC_FORCE_CALCULATION=core
export LANG=C.UTF-8
export LC_ALL=C.UTF-8

test -f "${source}" || fail source_missing
test ! -L "${source}" || fail source_symlink
test -f /oracle/fonts/fonts.conf || fail font_config_missing
test -d /oracle/fonts/fonts || fail font_directory_missing
test "$(wc -c < "${source}")" = "${RXLS_SOURCE_BYTES}" || fail source_size_mismatch
actual_source_sha256="$(sha256sum "${source}" | cut -d ' ' -f 1)"
test "${actual_source_sha256}" = "${RXLS_SOURCE_SHA256}" || fail source_hash_mismatch
test -z "$(find /oracle/evidence -mindepth 1 -print -quit)" || fail evidence_not_empty

# RLIMIT_FSIZE is a second bound in addition to the size-capped evidence tmpfs.
file_blocks="$(( (RXLS_EVIDENCE_MAX_BYTES + 511) / 512 ))"
ulimit -f "${file_blocks}" || fail fsize_limit

/opt/libreoffice26.2/program/soffice \
    --headless \
    --invisible \
    --nologo \
    --nodefault \
    --nofirststartwizard \
    --norestore \
    --nolockcheck \
    "-env:UserInstallation=file://${profile}" \
    --convert-to "${export_filter}" \
    --outdir /oracle/evidence \
    "${source}" \
    >"${runtime}/soffice.stdout" \
    2>"${runtime}/soffice.stderr" \
    || fail libreoffice_failed

generated="/oracle/evidence/input.pdf"
test -s "${generated}" || fail pdf_missing
mv "${generated}" "${pdf}"
pdf_bytes="$(wc -c < "${pdf}")"
pdf_sha256="$(sha256sum "${pdf}" | cut -d ' ' -f 1)"

cat > /oracle/evidence/oracle-manifest.json <<EOF
{
  "artifact": {
    "bytes": ${pdf_bytes},
    "path": "oracle/oracle.pdf",
    "sha256": "${pdf_sha256}"
  },
  "export": {
    "filter": "calc_pdf_Export",
    "single_page_sheets": ${single_page_sheets}
  },
  "font_pack_sha256": "${RXLS_FONT_PACK_SHA256}",
  "lock_sha256": "${RXLS_LOCK_SHA256}",
  "oracle": {
    "artifact_sha256": "18838cb9d028b664a9d0e966cd4c8ca47ca3ea363c393b41d1b5124740b121a5",
    "name": "LibreOffice",
    "version": "26.2.3.2"
  },
  "schema": "rxls.render-oracle-container-output.v2",
  "source": {
    "bytes": ${RXLS_SOURCE_BYTES},
    "path": "source/input${RXLS_SOURCE_EXTENSION}",
    "sha256": "${RXLS_SOURCE_SHA256}"
  }
}
EOF

chmod 0444 "${pdf}" /oracle/evidence/oracle-manifest.json

# Stream a deterministic archive before the evidence tmpfs is destroyed. No
# diagnostic output is mixed into stdout.
exec tar \
    --create \
    --file=- \
    --directory=/oracle/evidence \
    --format=ustar \
    --sort=name \
    --mtime='UTC 1970-01-01' \
    --owner=0 \
    --group=0 \
    --numeric-owner \
    oracle-manifest.json oracle.pdf
