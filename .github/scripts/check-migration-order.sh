#!/usr/bin/env bash
set -euo pipefail
IFS=$'\n\t'

base_ref="${1:-origin/${GITHUB_BASE_REF:-main}}"
failed=0

if ! git rev-parse --verify --quiet "${base_ref}^{commit}" >/dev/null; then
  echo "Base ref '${base_ref}' does not exist. Fetch it before running this check." >&2
  exit 1
fi

extract_versions() {
  local regex="$1"
  while IFS= read -r path; do
    local file="${path##*/}"
    if [[ "${file}" =~ ${regex} ]]; then
      printf '%s %s\n' "${BASH_REMATCH[1]}" "${path}"
    fi
  done
}

version_number() {
  local version="$1"
  echo $((10#${version}))
}

check_checksum_manifest() {
  local label="$1"
  local dir="$2"
  local extension="$3"
  local algorithm="$4"
  local manifest="$5"
  local manifest_failed=0

  if [[ ! -f "${manifest}" ]]; then
    failed=1
    echo "::error title=${label} checksum manifest missing::Expected ${manifest}"
    return
  fi

  if ! shasum -a "${algorithm}" --check "${manifest}"; then
    failed=1
    manifest_failed=1
    echo "::error title=${label} checksum mismatch::Release migrations must match ${manifest}"
  fi

  local duplicate_paths
  duplicate_paths="$(awk 'NF { print $2 }' "${manifest}" | sort | uniq -d)"
  if [[ -n "${duplicate_paths}" ]]; then
    failed=1
    manifest_failed=1
    echo "::error title=${label} duplicate checksum entries::Each migration path must appear once in ${manifest}"
    printf '%s\n' "${duplicate_paths}"
  fi

  local unlisted_paths
  unlisted_paths="$({
    comm -23 \
      <(find "${dir}" -maxdepth 1 -type f -name "*.${extension}" -print | sort) \
      <(awk 'NF { print $2 }' "${manifest}" | sort)
  })"
  if [[ -n "${unlisted_paths}" ]]; then
    failed=1
    manifest_failed=1
    echo "::error title=${label} migration missing checksum::Append checksums for every new migration to ${manifest}"
    printf '%s\n' "${unlisted_paths}"
  fi

  if git cat-file -e "${base_ref}:${manifest}" 2>/dev/null; then
    while IFS= read -r applied_entry; do
      [[ -n "${applied_entry}" ]] || continue
      if ! grep -Fqx -- "${applied_entry}" "${manifest}"; then
        failed=1
        manifest_failed=1
        echo "::error title=${label} applied migration changed::Base checksum entry must remain byte-for-byte: ${applied_entry}"
      fi
    done < <(git show "${base_ref}:${manifest}")
  fi

  if [[ "${manifest_failed}" -eq 0 ]]; then
    echo "${label}: migration checksums match the immutable release manifest."
  fi
}

check_migrations() {
  local label="$1"
  local dir="$2"
  local regex="$3"
  local manifest="$4"
  local dir_failed=0
  local bootstrapping_manifest=0

  if ! git cat-file -e "${base_ref}:${manifest}" 2>/dev/null; then
    bootstrapping_manifest=1
  fi

  local head_entries
  head_entries="$(
    find "${dir}" -maxdepth 1 -type f -print | sort | extract_versions "${regex}"
  )"

  local duplicate_versions
  duplicate_versions="$(
    printf '%s\n' "${head_entries}" | awk 'NF { print $1 }' | sort | uniq -d
  )"

  if [[ -n "${duplicate_versions}" ]]; then
    failed=1
    dir_failed=1
    echo "::error title=${label} duplicate migration versions::Duplicate migration version prefixes found in ${dir}"
    while IFS= read -r version; do
      [[ -n "${version}" ]] || continue
      echo "  ${version}:"
      printf '%s\n' "${head_entries}" | awk -v version="${version}" '$1 == version { print "    " $2 }'
    done <<<"${duplicate_versions}"
  fi

  local base_entries
  base_entries="$(
    git ls-tree -r --name-only "${base_ref}" -- "${dir}" | sort | extract_versions "${regex}"
  )"

  local base_max
  base_max="$(
    printf '%s\n' "${base_entries}" | awk 'NF { print $1 }' | sort -n | tail -n 1
  )"

  if [[ -z "${base_max}" ]]; then
    echo "${label}: no base migrations found under ${dir}; skipping monotonic version check."
    return
  fi

  local added_versions
  added_versions="$(
    comm -23 \
      <(printf '%s\n' "${head_entries}" | awk 'NF { print $1 }' | sort -u) \
      <(printf '%s\n' "${base_entries}" | awk 'NF { print $1 }' | sort -u)
  )"

  while IFS= read -r version; do
    [[ -n "${version}" ]] || continue
    if (( $(version_number "${version}") <= $(version_number "${base_max}") )) \
      && [[ "${bootstrapping_manifest}" -eq 0 ]]; then
      failed=1
      dir_failed=1
      echo "::error title=${label} non-monotonic migration::New migration version ${version} must be greater than ${base_max} from ${base_ref}"
      printf '%s\n' "${head_entries}" | awk -v version="${version}" '$1 == version { print "  " $2 }'
    fi
  done <<<"${added_versions}"

  if [[ "${dir_failed}" -eq 0 ]]; then
    if [[ "${bootstrapping_manifest}" -eq 1 ]]; then
      echo "${label}: bootstrapping the immutable migration lineage manifest."
    else
      echo "${label}: migration versions are monotonic relative to ${base_ref}."
    fi
  fi
}

check_migrations \
  "SQLx" \
  "services/api-rs/crates/centaur-session-sqlx/migrations" \
  '^([0-9]+)_.+\.sql$' \
  "services/api-rs/crates/centaur-session-sqlx/migrations/.checksums.sha384"

check_migrations \
  "Rails console" \
  "services/console/db/migrate" \
  '^([0-9]+)_.+\.rb$' \
  "services/console/db/migrate/.checksums.sha256"

check_checksum_manifest \
  "SQLx" \
  "services/api-rs/crates/centaur-session-sqlx/migrations" \
  "sql" \
  "384" \
  "services/api-rs/crates/centaur-session-sqlx/migrations/.checksums.sha384"

check_checksum_manifest \
  "Rails console" \
  "services/console/db/migrate" \
  "rb" \
  "256" \
  "services/console/db/migrate/.checksums.sha256"

exit "${failed}"
