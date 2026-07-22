
## candidate: thompson (thompson)
scope: exploratory_per_candidate
baseline_impl: naive
hypothesis: Pike VM bounds the live-thread set, removing exponential retry on alternation (catastrophic backtracking).
commit: 5122cc9  vcpus: 4
PLAN: measure=scan main(count=8377 sessions=30 blocks=8) pathological(count=150 sessions=3 blocks=6) order_seed=1 seeds=[42]
gate: main=PASS pathological=PASS
main: lower_95_ratio=0.564236 speedup_percent=-42.253 evidence=screen_inconclusive
pathological: lower_95_ratio=101509.977745 speedup_percent=12016142.275 evidence=screen_positive
Decision: rejected (main_95>1=false pathological_95>1=true)

## candidate: thompson-heldout (thompson)
scope: held_out_confirmation
baseline_impl: naive
hypothesis: Pre-registered confirmation of the pathological-corpus win with seeds/order-seed not used in exploration.
commit: 5122cc9  vcpus: 4
PLAN: measure=scan main(count=8377 sessions=30 blocks=8) pathological(count=150 sessions=3 blocks=6) order_seed=77 seeds=[911]
gate: main=PASS pathological=PASS
main: lower_95_ratio=0.515831 speedup_percent=-44.670 evidence=inconclusive
pathological: lower_95_ratio=130025.417544 speedup_percent=13901334.324 evidence=candidate_faster
Decision: rejected (main_95>1=false pathological_95>1=true)

## candidate: prefilter (prefilter)
scope: exploratory_per_candidate
baseline_impl: naive
hypothesis: SIMD literal-prefix scan (memchr) jumps to candidate starts; anchored linear verify. Removes per-byte walk on literal-leading patterns.
commit: 5122cc9  vcpus: 4
PLAN: measure=scan main(count=8377 sessions=30 blocks=8) pathological(count=150 sessions=3 blocks=6) order_seed=1 seeds=[42]
gate: main=PASS pathological=PASS
main: lower_95_ratio=2.235791 speedup_percent=128.021 evidence=screen_positive
pathological: lower_95_ratio=129885.943128 speedup_percent=13538330.862 evidence=screen_positive
Decision: promoted (main_95>1=true pathological_95>1=true)

## candidate: prefilter-heldout (prefilter)
scope: held_out_confirmation
baseline_impl: naive
hypothesis: Pre-registered confirmation of the main+pathological win with seeds/order-seed not used in exploration.
commit: 5122cc9  vcpus: 4
PLAN: measure=scan main(count=8377 sessions=30 blocks=8) pathological(count=150 sessions=3 blocks=6) order_seed=77 seeds=[911]
gate: main=PASS pathological=PASS
main: lower_95_ratio=2.062712 speedup_percent=123.109 evidence=candidate_faster
pathological: lower_95_ratio=118341.626058 speedup_percent=12717034.339 evidence=candidate_faster
Decision: promoted (main_95>1=true pathological_95>1=true)
