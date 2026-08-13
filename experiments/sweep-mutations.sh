#!/usr/bin/env bash
#
# Shows that the tests which generate their own inputs would notice.
#
# A property sweep or a stress run says something about the code only once it
# has been seen to fail. Two of these found a bug when they were written and
# are their own evidence; the rest have never failed, and a sweep that generates
# nothing reads exactly the same from the outside as one that generates
# everything.
#
# So this puts a bug back. Each mutation below is one edit to the code under a
# test — the kind of edit a refactor makes by accident, not a `panic!` dropped
# in to be found — and each is followed by the test that owns it, which is
# expected to fail. A mutation the test survives is a hole in the test, and the
# script says so and exits non-zero.
#
# The edit is applied to the working tree and undone afterwards, including on
# ^C. If something kills it harder than that, `git checkout -- crates` puts the
# sources back.
#
# Usage: experiments/sweep-mutations.sh [name ...]   (default: all of them)

set -uo pipefail

cd "$(dirname "$0")/.."

logs=target/sweep-mutations
mkdir -p "$logs"

wanted=("$@")
touched=()
failures=0
ran=0

restore() {
    for file in "${touched[@]:-}"; do
        [[ -n "$file" ]] || continue
        mv -f "$logs/$(basename "$file").orig" "$file"
        # `mv` carries the backup's timestamp over, which is older than the
        # object file built from the mutation — so cargo would keep the mutated
        # build and every test run after this one would be a lie.
        touch "$file"
    done
    touched=()
}
trap 'restore; exit 130' INT TERM
trap restore EXIT

# mutation <name> <file> <package> <test binary> <test> <what it must catch>
#          <the line as written> <the line with the bug in>
mutation() {
    local name=$1 file=$2 package=$3 binary=$4 test=$5 catches=$6 from=$7 to=$8

    if ((${#wanted[@]})) && ! [[ " ${wanted[*]} " == *" $name "* ]]; then
        return 0
    fi
    ran=$((ran + 1))

    cp "$file" "$logs/$(basename "$file").orig"
    touched=("$file")

    # Exactly once, or the test would be run against code nobody edited: a
    # mutation that no longer applies is the failure this check is here for.
    if ! FROM="$from" TO="$to" perl -0pi -e '
        my $hits = s/\Q$ENV{FROM}\E/$ENV{TO}/g;
        die "the line to mutate appears $hits times, not once\n" unless $hits == 1;
    ' "$file"; then
        echo "FAILED: $name no longer applies to $file"
        failures=$((failures + 1))
        restore
        return 0
    fi

    echo "== $name: $catches"
    echo "   $file: $from"
    echo "               -> $to"

    cargo test --package "$package" --test "$binary" -- --exact "$test" \
        >"$logs/$name.log" 2>&1
    local outcome=$?
    restore

    if ((outcome != 0)); then
        # What the test had to say, which is the first panic and no more: the
        # shrunk move list under it is what the log is for.
        echo "   ok: $test failed with"
        awk '/panicked at/ { inside = 1 }
             /minimal failing input/ { exit }
             inside { print; if (++lines == 5) exit }' \
            "$logs/$name.log" | cut -c1-120 | sed 's/^/       /'
    else
        echo "   FAILED: $test passed with the bug in — see $logs/$name.log"
        failures=$((failures + 1))
    fi
    echo
}

mutation quarantine \
    crates/molt-arch/src/va.rs molt-arch va_churn churn_hands_out_no_address_twice \
    "an address handed out again before every hart flushed it" \
    '.find(|&index| self.holes[index].ready <= retired && self.holes[index].bytes() >= size)' \
    '.find(|&index| self.holes[index].bytes() >= size)'

mutation split-counts \
    crates/molt-arch/src/refcount.rs molt-arch refcount_churn \
    every_request_leaves_the_counts_the_model_expects \
    "a split that hands the children a count nobody held" \
    'Self::run(leaf, child, Class::FANOUT, run.count)?' \
    'Self::run(leaf, child, Class::FANOUT, 1)?'

mutation flush-set \
    crates/molt-arch/src/shootdown.rs molt-arch shootdown_churn \
    no_run_of_answers_leaves_a_round_nobody_can_close \
    "a round that forgets the cores which already answered" \
    'self.flushed |= 1 << cpu.index();' \
    'self.flushed = 1 << cpu.index();'

mutation tail-slack \
    crates/molt-abi/src/ring.rs molt-abi ring_churn lying_producer_reaches_fault \
    "a producer allowed one slot more than the ring has" \
    'if ready as usize > N {' \
    'if ready as usize > N + 1 {'

mutation ipv4-total \
    crates/molt-net/src/ipv4.rs molt-net frame_churn ipv4_stays_inside_input \
    "a length field trusted past the end of the frame" \
    'if total < header || bytes.len() < total {' \
    'if total < header {'

mutation holder-count \
    crates/molt-arch/src/cache.rs molt-arch contention \
    eight_cores_share_one_space_without_losing_an_address \
    "a window handed to a core without counting the holder" \
    'window.holders = window.holders.checked_add(1).ok_or(Error::Saturated)?;' \
    'window.holders = window.holders.checked_add(0).ok_or(Error::Saturated)?;'

if ((failures)); then
    echo "$failures of $ran mutations went unnoticed"
    exit 1
fi
echo "all $ran mutations were caught by the test that owns them"
