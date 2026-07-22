#!/usr/bin/env bash
# ABOUTME: Builds and trains a reproducible profile-guided optiwork release bench.
# ABOUTME: Trains the bench binary across impls × corpora (naive is excluded from the
# ABOUTME: pathological corpus — it is catastrophically slow there), merges the
# ABOUTME: profiles with rustc's llvm-profdata, and rebuilds with profile-use.
# ABOUTME: Instrumented data and optimized artifacts live in an isolated run directory.

set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
run_name=${1:-manual}
run_root="$repo_root/target/pgo/$run_name"
profiles_root="$run_root/profiles"
instrumented_target="$run_root/instrumented"
optimized_target="$run_root/optimized"
merged_profile="$run_root/optiwork.profdata"
manifest="$run_root/manifest.txt"

training_seed=42
training_sessions=8

# Training matrix: <impl> <corpus-stem>. naive is omitted from pathological because
# it is exponential there and would never finish a training run.
training_matrix=(
    naive        main
    thompson     main
    thompson     pathological
    prefilter    main
    prefilter    pathological
)

if [[ -e "$run_root" ]]; then
    echo "PGO run directory already exists: $run_root" >&2
    echo "Choose a different run name." >&2
    exit 2
fi

# Corpora must exist (run `cargo run -p regexbench --features oracle --example
# gen_golden -- corpora` first). PGO does not regenerate them.
for stem in main pathological; do
    if [[ ! -f "$repo_root/corpora/$stem.bin" ]]; then
        echo "missing corpora/$stem.bin — regenerate corpora before PGO" >&2
        exit 2
    fi
done

host=$(rustc -vV | sed -n 's/^host: //p')
llvm_profdata="$(rustc --print sysroot)/lib/rustlib/$host/bin/llvm-profdata"
if [[ ! -x "$llvm_profdata" ]]; then
    echo "rustc's llvm-profdata was not found at $llvm_profdata" >&2
    exit 2
fi

mkdir -p "$profiles_root"

# Pin the corpus byte counts up front with a NON-instrumented bench. Doing this
# with the instrumented binary would emit a stray profraw into $profiles_root and
# break the exact-count invariant below.
release_bench="$repo_root/target/release/examples/bench"
(
    cd "$repo_root"
    cargo build --release -p regexbench --example bench >/dev/null
)
main_count=$("$release_bench" --count-of "$repo_root/corpora/main.bin")
path_count=$("$release_bench" --count-of "$repo_root/corpora/pathological.bin")

source_revision=$(git -C "$repo_root" rev-parse --verify HEAD 2>/dev/null || true)
source_revision=${source_revision:-unknown}
if [[ "$source_revision" == unknown ]]; then
    source_state=unknown
elif [[ -n "$(git -C "$repo_root" status --porcelain --untracked-files=normal)" ]]; then
    source_state=dirty
else
    source_state=clean
fi
rustc_version=$(rustc --version)
cargo_version=$(cargo --version)
llvm_profdata_version=$(
    "$llvm_profdata" --version | sed -n 's/^[[:space:]]*//; /LLVM version/p'
)

# Collapse the matrix into "impl/corpus" pairs for the manifest.
pairs=()
for ((i = 0; i < ${#training_matrix[@]}; i += 2)); do
    pairs+=("${training_matrix[$i]}/${training_matrix[$((i + 1))]}")
done

{
    printf 'format=optiwork-pgo-v1\n'
    printf 'source_revision=%s\n' "$source_revision"
    printf 'source_state=%s\n' "$source_state"
    printf 'rustc=%s\n' "$rustc_version"
    printf 'host=%s\n' "$host"
    printf 'cargo=%s\n' "$cargo_version"
    printf 'llvm_profdata=%s\n' "$llvm_profdata_version"
    printf 'training_pairs=%s\n' "${pairs[*]}"
    printf 'seed=%s\n' "$training_seed"
    printf 'sessions=%s\n' "$training_sessions"
    printf 'command=bench --measure scan --impl <impl> --corpus <stem>.bin --seed %s --sessions %s --count <stem-bytes>\n' \
        "$training_seed" "$training_sessions"
} >"$manifest"

echo "Building instrumented release bench"
(
    cd "$repo_root"
    CARGO_TARGET_DIR="$instrumented_target" \
        RUSTFLAGS="-Cprofile-generate=$profiles_root" \
        cargo build --release -p regexbench --example bench
)

# Cargo may execute instrumented build scripts while compiling. Their default
# profiles describe the build rather than the bench workload, so drop them before
# collecting the deliberately named training profiles below.
find "$profiles_root" -maxdepth 1 -type f -name '*.profraw' -delete

bench="$instrumented_target/release/examples/bench"

echo "Training across impls × corpora"
for ((i = 0; i < ${#training_matrix[@]}; i += 2)); do
    impl="${training_matrix[$i]}"
    stem="${training_matrix[$((i + 1))]}"
    case "$stem" in
        main) count=$main_count ;;
        pathological) count=$path_count ;;
        *) echo "unknown corpus stem: $stem" >&2; exit 2 ;;
    esac
    echo "  train: impl=$impl corpus=$stem count=$count"
    LLVM_PROFILE_FILE="$profiles_root/$impl-$stem-%m.profraw" \
        "$bench" --measure scan --impl "$impl" \
        --corpus "$repo_root/corpora/$stem.bin" \
        --seed "$training_seed" --sessions "$training_sessions" \
        --count "$count" >/dev/null
done

mapfile -t training_profiles < <(
    find "$profiles_root" -maxdepth 1 -type f -name '*.profraw' -print | sort
)
expected_profiles=$(( ${#training_matrix[@]} / 2 ))
if (( ${#training_profiles[@]} != expected_profiles )); then
    echo "Expected $expected_profiles training profiles, found ${#training_profiles[@]}" >&2
    exit 2
fi

"$llvm_profdata" merge --failure-mode=all \
    -o "$merged_profile" "${training_profiles[@]}"

echo "Building profile-optimized release bench"
(
    cd "$repo_root"
    CARGO_TARGET_DIR="$optimized_target" \
        RUSTFLAGS="-Cprofile-use=$merged_profile -Cllvm-args=-pgo-warn-missing-function" \
        cargo build --release -p regexbench --example bench
)

echo "PGO profile: $merged_profile"
echo "Training manifest: $manifest"
echo "Optimized bench: $optimized_target/release/examples/bench"
