#!/usr/bin/env bash
# Build the pinned daemon with extended FUSE metadata support.
set -Eeuo pipefail
script_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
artifact_root=$(realpath -m -- "${1:?build directory required}")
build_jobs=${2:-8}
version=1.14.0
archive_sha=7051accc9cf2816e25945c8216e6d4be2b4e75fc5e77559cd7eed5e400245cc1
patch_file=$script_root/patches/virtiofsd-statx.patch
cache=$artifact_root/cache
source_dir=$cache/virtiofsd-$version-statx
archive=$cache/virtiofsd-$version.crate
daemon=$artifact_root/virtiofsd/bin/virtiofsd
stamp=$artifact_root/virtiofsd/statx.inputs
log() { printf '[build] stage=virtiofsd %s\n' "$*" >&2; }
trap 'log "result=failed stage_line=$LINENO exit=$?"' ERR
mkdir -p -- "$cache" "$(dirname -- "$daemon")" "$artifact_root/bin"
recipe=$(sha256sum -- "$patch_file" "$0" | cut -d ' ' -f 1 | sha256sum | cut -d ' ' -f 1)

reuse() {
    [[ -x $daemon && -f $stamp ]] || return 1
    local expected artifact_digest
    read -r expected artifact_digest < "$stamp"
    [[ $expected == "$recipe" && $artifact_digest == "$(sha256sum -- "$daemon" | cut -d ' ' -f 1)" ]]
}
prepare_source() {
    if [[ ! -f $archive ]]; then
        log "stage=download version=$version"
        curl --fail --location --retry 3 --connect-timeout 15 --max-time 120 \
            "https://static.crates.io/crates/virtiofsd/virtiofsd-$version.crate" -o "$archive.part"
        mv -- "$archive.part" "$archive"
    fi
    log "stage=verify-source version=$version"
    printf '%s  %s\n' "$archive_sha" "$archive" | sha256sum --check
    local staging
    staging=$(mktemp -d "$cache/virtiofsd-source.XXXXXX")
    tar -xf "$archive" --strip-components=1 -C "$staging"
    patch --batch --forward -d "$staging" -p1 < "$patch_file"
    rm -rf -- "$source_dir"
    mv -- "$staging" "$source_dir"
}
if reuse; then
    log 'stage=compile result=reuse reason=recipe-and-artifact-match'
else
    prepare_source
    log "stage=compile version=$version fuse_statx=true"
    cargo build --locked --release --jobs "$build_jobs" --manifest-path "$source_dir/Cargo.toml" \
        --target-dir "$artifact_root/virtiofsd/target"
    install -m 0755 "$artifact_root/virtiofsd/target/release/virtiofsd" "$daemon.part"
    mv -- "$daemon.part" "$daemon"
    printf '%s %s\n' "$recipe" "$(sha256sum -- "$daemon" | cut -d ' ' -f 1)" > "$stamp.part"
    mv -- "$stamp.part" "$stamp"
fi
install -m 0755 "$daemon" "$artifact_root/bin/virtiofsd.part"
mv -- "$artifact_root/bin/virtiofsd.part" "$artifact_root/bin/virtiofsd"
log 'stage=artifact result=success capabilities=statx'
