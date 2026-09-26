#!/usr/bin/env bash
# Restore the canonical directory enumeration on fresh, ephemeral hosted storage.
# Only Docker's volumes directory is temporarily rebound; image data is untouched.
set -Eeuo pipefail
umask 077
export LANG=C LC_ALL=C

DOCKER_ROOT=/var/lib/docker
SEED=00000000-0000-0000-0000-000000000014
FS_UUID=27e2ccf5-fd0a-452d-bb3f-69d80d5bd70d
SIZE=12884901888
NAMES=(
  4c599c202bc5c08e2d34565a40eac3b2-le64.cache-8
  7ef2298fde41cc6eeb7af42e48b7d293-le64.cache-8
  d589a48862398ed80a3d6066f4f56f4c-le64.cache-8
  3830d5c3ddfd5cd38a049b759396e72e-le64.cache-8
)

die() { printf 'oracle_storage_error:%s\n' "$*" >&2; exit 1; }
run() { timeout --signal=TERM --kill-after=5 30 "$@"; }
capture() {
  local output
  output="$(run "$@" | head -c 65537)" || die "command_failed:$1"
  [[ ${#output} -le 65536 ]] || die "command_output_limit:$1"
  printf '%s' "$output"
}
safe_directory() {
  local path="$1" owner="$2" current mode
  [[ "$path" == /* && "$path" != / && "$path" != *[$'\n\r\t '\\\"\']* ]] || die unsafe_path
  [[ "/$path/" != */../* && "/$path/" != */./* ]] || die unsafe_path
  current="$path"
  while [[ "$current" != / ]]; do
    [[ -d "$current" && ! -L "$current" ]] || die path_type
    current="${current%/*}"
    [[ -n "$current" ]] || current=/
  done
  [[ "$(capture stat -c %u "$path")" == "$owner" ]] || die path_owner
  mode="$(capture stat -c %a "$path")" || die path_mode_query
  [[ "$mode" =~ ^[0-7]{3,4}$ ]] || die path_mode
  (( (8#$mode & 8#022) == 0 )) || die path_mode
}
safe_file() {
  local path="$1"
  [[ -f "$path" && ! -L "$path" ]] || die file_type
  [[ "$(capture stat -c '%u %h' "$path")" == '0 1' ]] || die file_owner
}
is_mounted() {
  local status
  if run mountpoint -q "$1"; then return 0; else status=$?; fi
  [[ "$status" == 32 || "$status" == 1 ]] || die mountpoint_failed
  return 1
}
storage_identity() {
  local identity
  identity="$(capture findmnt -rn -T "$VOLUMES" -o SOURCE,FSROOT,FSTYPE,MAJ:MIN)" || die storage_identity_query
  [[ "$identity" =~ ^[^[:space:]]+[[:space:]]+[^[:space:]]+[[:space:]]+(ext4|xfs)[[:space:]]+[0-9]+:[0-9]+$ ]] || die storage_identity
  printf '%s' "$identity"
}
original_storage() {
  local current saved
  ! is_mounted "$VOLUMES" || die unexpected_volumes_mount
  current="$(storage_identity)" || die storage_identity_query
  saved="$(capture cat "$STATE/original-storage")" || die saved_identity_query
  [[ "$current" == "$saved" ]] || die original_storage_changed
}
context() {
  [[ "$(capture id -u)" == 0 ]] || die root_required
  [[ "${GITHUB_ACTIONS-}" == true && "${RUNNER_ENVIRONMENT-}" == github-hosted && "${RUNNER_OS-}" == Linux ]] || die hosted_context
  [[ "$(capture uname -m)" == x86_64 && "$(capture lsb_release -is)" == Ubuntu && "$(capture lsb_release -rs)" == 24.04 ]] || die hosted_platform
  [[ "${GITHUB_RUN_ID-}" =~ ^[0-9]{1,20}$ && "${GITHUB_RUN_ATTEMPT-}" =~ ^[0-9]{1,8}$ && "${GITHUB_JOB-}" =~ ^[A-Za-z0-9_-]{1,64}$ ]] || die job_identity
  [[ "${SUDO_UID-0}" =~ ^[0-9]{1,10}$ ]] || die caller_identity
  [[ -n "${RUNNER_TEMP-}" ]] || die runner_temp
  safe_directory "$RUNNER_TEMP" "${SUDO_UID-0}"
  [[ -z "${DOCKER_CONTEXT-}" || "${DOCKER_CONTEXT-}" == default ]] || die docker_context
  [[ -z "${DOCKER_HOST-}" || "${DOCKER_HOST-}" == unix:///var/run/docker.sock ]] || die docker_endpoint
  [[ "$(capture docker context inspect default --format '{{.Endpoints.docker.Host}}')" == unix:///var/run/docker.sock ]] || die docker_endpoint
  safe_directory "$DOCKER_ROOT" 0
  VOLUMES="$DOCKER_ROOT/volumes"
  safe_directory "$VOLUMES" 0
  STATE="$RUNNER_TEMP/rxls-oracle-build-storage-${GITHUB_RUN_ID}-${GITHUB_RUN_ATTEMPT}-${GITHUB_JOB}"
  IMAGE="$STATE/canonical.ext4"
  MOUNT="$STATE/mount"
  MAGIC="rxls.oracle-build-storage.v1:${GITHUB_RUN_ID}:${GITHUB_RUN_ATTEMPT}:${GITHUB_JOB}"
}
idle_docker() {
  local containers volumes
  [[ "$(capture docker info --format '{{.DockerRootDir}} {{.Architecture}} {{.Driver}} {{.LiveRestoreEnabled}}')" == "$DOCKER_ROOT x86_64 overlay2 false" ]] || die daemon_identity
  containers="$(capture docker ps -aq)" || die container_inventory
  volumes="$(capture docker volume ls -q)" || die volume_inventory
  [[ -z "$containers" ]] || die containers_present
  [[ -z "$volumes" ]] || die volumes_present
}
stop_docker() {
  local tasks
  run systemctl stop docker.socket docker.service || die daemon_stop
  [[ "$(capture systemctl show -p ActiveState --value docker.socket)" == inactive ]] || die socket_active
  [[ "$(capture systemctl show -p ActiveState --value docker.service)" == inactive ]] || die daemon_active
  tasks="$(capture ctr --namespace moby tasks list -q)" || die containerd_query
  [[ -z "$tasks" ]] || die containerd_tasks
}
start_docker() {
  run systemctl start docker.socket docker.service || die daemon_start
  idle_docker
}
owned_image_file() {
  local bytes
  safe_file "$IMAGE"
  bytes="$(capture stat -c %s "$IMAGE")" || die image_size_query
  [[ "$bytes" =~ ^[0-9]{1,11}$ ]] || die image_size
  (( bytes <= SIZE )) || die image_size
}
image_file() {
  owned_image_file
  [[ "$(capture stat -c %s "$IMAGE")" == "$SIZE" ]] || die image_size
}
attached_loop() {
  local devices
  devices="$(capture losetup --associated "$IMAGE" --noheadings --output NAME)" || die loop_query
  [[ -z "$devices" || "$devices" =~ ^/dev/loop[0-9]+$ ]] || die loop_inventory
  if [[ -n "$devices" ]]; then
    [[ "$(capture losetup --noheadings --output BACK-FILE "$devices")" == "$IMAGE" ]] || die loop_backing_file
  fi
  printf '%s' "$devices"
}
new_storage() {
  local device properties
  image_file
  safe_directory "$MOUNT" 0
  is_mounted "$MOUNT" || die image_not_mounted
  [[ "$(capture findmnt -rn -M "$MOUNT" -o FSTYPE,FSROOT)" == 'ext4 /' ]] || die image_filesystem
  device="$(capture findmnt -rn -M "$MOUNT" -o SOURCE)" || die mount_source_query
  [[ "$device" =~ ^/dev/loop[0-9]+$ && "$(attached_loop)" == "$device" ]] || die loop_device
  [[ "$(capture blkid -s UUID -o value "$device")" == "$FS_UUID" ]] || die filesystem_uuid
  properties="$(capture tune2fs -l "$device")" || die filesystem_query
  [[ "$properties" == *"Directory Hash Seed:      $SEED"* && "$properties" == *'Default directory hash:   half_md4'* ]] || die directory_hash
  [[ "$(printf '%s\n' "$properties" | grep '^Filesystem features:')" =~ (^|[[:space:]])dir_index($|[[:space:]]) ]] || die directory_index
}
bound_storage() {
  local bound_device mounted_device
  new_storage
  [[ "$(capture findmnt -rn -M "$VOLUMES" -o FSROOT)" == /volumes ]] || die bound_root
  bound_device="$(capture findmnt -rn -M "$VOLUMES" -o MAJ:MIN)" || die bound_device_query
  mounted_device="$(capture findmnt -rn -M "$MOUNT" -o MAJ:MIN)" || die mount_device_query
  [[ -n "$bound_device" && "$bound_device" == "$mounted_device" ]] || die bound_device
}
single_mount() {
  [[ "$(capture findmnt -rn -R "$1" -o TARGET)" == "$1" ]] || die nested_mounts
}
state_identity() {
  safe_directory "$STATE" 0
  [[ "$(capture stat -c %a "$STATE")" == 700 ]] || die state_mode
  safe_file "$STATE/owner"
  [[ "$(capture stat -c %s "$STATE/owner")" -le 256 ]] || die state_owner_size
  [[ "$(capture cat "$STATE/owner")" == "$MAGIC" ]] || die state_identity
  safe_file "$STATE/original-storage"
}
state_lock() {
  [[ "${LOCK_HELD-0}" != 1 ]] || return 0
  if [[ -e "$STATE/operation.lock" || -L "$STATE/operation.lock" ]]; then safe_file "$STATE/operation.lock"; fi
  exec 9> "$STATE/operation.lock"
  run flock -w 5 9 || die concurrent_operation
  LOCK_HELD=1
}
metadata_verified() {
  if [[ -e "$STATE/original-metadata-verified" || -L "$STATE/original-metadata-verified" ]]; then
    safe_file "$STATE/original-metadata.sha256"
    safe_file "$STATE/original-metadata-verified"
    [[ "$(capture cat "$STATE/original-metadata-verified")" == "$MAGIC" ]] || die metadata_verification_phase
    return 0
  fi
  return 1
}
verify_original_metadata() {
  original_storage
  if metadata_verified; then return 0; fi
  if [[ -e "$STATE/original-metadata.sha256" || -L "$STATE/original-metadata.sha256" ]]; then
    safe_file "$STATE/original-metadata.sha256"
    run sha256sum --check --strict "$STATE/original-metadata.sha256" || die original_metadata_changed
    # Docker legitimately rewrites its bbolt metadata when it starts. Record
    # this verification before that first startup; never rewrite the receipt.
    printf '%s\n' "$MAGIC" > "$STATE/original-metadata-verified" || die metadata_phase_write
  fi
}
restored_state() {
  local device
  original_storage
  [[ ! -e "$IMAGE" && ! -L "$IMAGE" ]] || die restored_image_present
  [[ ! -e "$MOUNT" && ! -L "$MOUNT" ]] || die restored_mount_present
  ! is_mounted "$MOUNT" || die restored_mount_present
  device="$(attached_loop)" || die loop_query
  [[ -z "$device" ]] || die restored_loop_present
  if metadata_verified; then
    :
  elif [[ -e "$STATE/original-metadata.sha256" || -L "$STATE/original-metadata.sha256" ]]; then
    die metadata_verification_missing
  fi
  # A completed restore is read-only: neither stop a now-reused daemon nor
  # compare its legitimately updated metadata to the pre-build receipt.
  printf 'oracle_storage_already_restored state=%s\n' "$STATE"
}
restore_state() {
  local service device result
  if [[ ! -e "$STATE" && ! -L "$STATE" ]]; then
    ! is_mounted "$VOLUMES" || die state_missing_with_volumes_mount
    printf '%s\n' 'oracle_storage_not_prepared'
    return
  fi
  state_identity
  state_lock
  if [[ -e "$STATE/result" || -L "$STATE/result" ]]; then
    safe_file "$STATE/result"
    result="$(capture cat "$STATE/result")" || die result_query
    case "$result" in
      restored) restored_state; return ;;
      active) ;;
      *) die result_state ;;
    esac
  fi
  # Refuse to hide or remove any workload. The workflow removes only its own
  # Buildx builders before calling restore; leaked resources stop cleanup.
  service="$(capture systemctl show -p ActiveState --value docker.service)" || die daemon_state_query
  case "$service" in active) idle_docker ;; inactive) ;; *) die daemon_state ;; esac
  if is_mounted "$VOLUMES"; then
    bound_storage
    safe_file "$STATE/original-metadata.sha256"
    [[ ! -e "$STATE/original-metadata-verified" && ! -L "$STATE/original-metadata-verified" ]] || die verified_metadata_bound
  else
    original_storage
    # A bind failure leaves the original daemon stopped. Verify its metadata
    # before restarting it to inventory persisted resources for safe cleanup.
    if [[ "$service" == inactive ]]; then verify_original_metadata; fi
  fi
  # A stopped daemon can still own persisted containers or volumes. Inventory
  # the validated current store before any unmount or backing-file deletion.
  # Starting it does not authorize deleting resources if the inventory fails.
  if [[ "$service" == inactive ]]; then start_docker; fi
  stop_docker
  if is_mounted "$VOLUMES"; then
    single_mount "$VOLUMES"
    run umount "$VOLUMES" || die volumes_unmount
  fi
  verify_original_metadata
  if is_mounted "$MOUNT"; then
    new_storage
    single_mount "$MOUNT"
    run umount "$MOUNT" || die image_unmount
  fi
  if [[ -e "$IMAGE" || -L "$IMAGE" ]]; then
    owned_image_file
    device="$(attached_loop)" || die loop_query
    if [[ -n "$device" ]]; then run losetup --detach "$device" || die loop_detach; fi
    device="$(attached_loop)" || die loop_query
    [[ -z "$device" ]] || die loop_attached
    run rm -- "$IMAGE" || die image_remove
  fi
  if [[ -e "$MOUNT" || -L "$MOUNT" ]]; then
    safe_directory "$MOUNT" 0
    run rmdir "$MOUNT" || die mount_directory_remove
  fi
  start_docker
  printf '%s\n' restored > "$STATE/result"
  printf 'oracle_storage_restored state=%s\n' "$STATE"
}
prepare() {
  local available device probe observed expected original
  idle_docker
  ! is_mounted "$VOLUMES" || die volumes_already_mounted
  [[ ! -e "$STATE" && ! -L "$STATE" ]] || die state_already_exists
  available="$(capture df --output=avail -B1 "$RUNNER_TEMP" | tail -n 1)" || die free_space_query
  [[ "$available" =~ ^[[:space:]]*[0-9]+$ ]] || die free_space_query
  (( available > 15032385536 )) || die free_space
  original="$(storage_identity)" || die storage_identity_query
  run mkdir -m 0700 "$STATE" || die state_create
  CREATED_STATE=1
  printf '%s\n' "$MAGIC" > "$STATE/owner" || die state_owner_write
  printf '%s\n' "$original" > "$STATE/original-storage" || die state_identity_write
  INITIALIZED_STATE=1
  state_lock
  run truncate -s "$SIZE" "$IMAGE" || die image_create
  image_file
  timeout --signal=TERM --kill-after=5 180 mkfs.ext4 -q -F -m 0 -U "$FS_UUID" \
    -E "lazy_itable_init=0,lazy_journal_init=0,hash_seed=$SEED" "$IMAGE" || die image_format
  run mkdir -m 0700 "$MOUNT" || die mount_directory_create
  device="$(capture losetup --find --show "$IMAGE")" || die loop_attach
  [[ "$device" =~ ^/dev/loop[0-9]+$ && "$(attached_loop)" == "$device" ]] || die loop_attach
  run mount -t ext4 "$device" "$MOUNT" || die image_mount
  new_storage
  run mkdir -m 0701 "$MOUNT/volumes" || die volume_directory_create
  probe="$(capture mktemp -d "$MOUNT/order-probe.XXXXXXXX")" || die probe_create
  [[ "$probe" == "$MOUNT"/order-probe.* ]] || die probe_path
  for i in 3 2 1 0; do : > "$probe/${NAMES[$i]}"; done
  observed="$(capture env LC_ALL=C ls -1U "$probe")" || die probe_query
  expected="$(printf '%s\n' "${NAMES[@]}")"
  [[ "$observed" == "$expected" ]] || die directory_order
  for name in "${NAMES[@]}"; do run rm -- "$probe/$name" || die probe_remove; done
  run rmdir "$probe" || die probe_remove
  idle_docker
  stop_docker
  safe_file "$VOLUMES/metadata.db"
  run sha256sum "$VOLUMES/metadata.db" > "$STATE/original-metadata.sha256" || die metadata_capture
  run mount --bind "$MOUNT/volumes" "$VOLUMES" || die volumes_bind
  bound_storage
  start_docker
  printf '%s\n' active > "$STATE/result"
  printf 'oracle_storage_active seed=%s state=%s\n' "$SEED" "$STATE"
}
on_exit() {
  local status="$1" cleanup_status
  trap - EXIT INT TERM
  if [[ "$status" != 0 && "${MODE-}" == prepare && "${CREATED_STATE-0}" == 1 ]]; then
    printf 'oracle_storage_prepare_failed status=%s; attempting owned-state rollback\n' "$status" >&2
    set +e
    (
      set -Eeuo pipefail
      if [[ "${INITIALIZED_STATE-0}" == 1 ]]; then
        restore_state
      else
        # No image/loop/daemon mutation occurs before both receipts exist.
        # Only this process knows it freshly created an incomplete state dir.
        safe_directory "$STATE" 0
        for file in owner original-storage; do
          if [[ -e "$STATE/$file" || -L "$STATE/$file" ]]; then
            safe_file "$STATE/$file"
            run rm -- "$STATE/$file" || die partial_state_remove
          fi
        done
        run rmdir "$STATE" || die partial_state_remove
      fi
    )
    cleanup_status=$?
    printf 'oracle_storage_rollback_status:%s\n' "$cleanup_status" >&2
  fi
  exit "$status"
}
main() {
  [[ $# == 1 && "$1" =~ ^(prepare|restore)$ ]] || die usage
  MODE="$1"
  CREATED_STATE=0
  INITIALIZED_STATE=0
  LOCK_HELD=0
  context
  trap 'on_exit $?' EXIT
  trap 'exit 130' INT
  trap 'exit 143' TERM
  case "$MODE" in prepare) prepare ;; restore) restore_state ;; esac
}
if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then main "$@"; fi
