
## candidate: thompson (thompson)
scope: exploratory_per_candidate
baseline_impl: naive
hypothesis: Pike VM bounds the live-thread set, removing exponential retry on alternation (catastrophic backtracking).
commit:   vcpus: 4
PLAN: measure=scan main(count=8377 sessions=30 blocks=8) pathological(count=150 sessions=3 blocks=6) order_seed=1 seeds=[42]
gate: main=PASS pathological=PASS
main: lower_95_ratio=0.570285 speedup_percent=-41.594 evidence=screen_inconclusive
pathological: lower_95_ratio=128413.822464 speedup_percent=13349664.611 evidence=screen_positive
Decision: rejected (main_95>1=false pathological_95>1=true)

## candidate: thompson-heldout (thompson)
scope: held_out_confirmation
baseline_impl: naive
hypothesis: Pre-registered confirmation of the pathological-corpus win with seeds/order-seed not used in exploration.
commit:   vcpus: 4
PLAN: measure=scan main(count=8377 sessions=30 blocks=8) pathological(count=150 sessions=3 blocks=6) order_seed=77 seeds=[911]
gate: main=PASS pathological=PASS
main: lower_95_ratio=0.562808 speedup_percent=-41.809 evidence=inconclusive
pathological: lower_95_ratio=124129.824657 speedup_percent=13143611.106 evidence=candidate_faster
Decision: rejected (main_95>1=false pathological_95>1=true)

## candidate: prefilter (prefilter)
scope: exploratory_per_candidate
baseline_impl: naive
hypothesis: SIMD literal-prefix scan (memchr) jumps to candidate starts; anchored linear verify. Removes per-byte walk on literal-leading patterns.
commit:   vcpus: 4
PLAN: measure=scan main(count=8377 sessions=30 blocks=8) pathological(count=150 sessions=3 blocks=6) order_seed=1 seeds=[42]
gate: main=PASS pathological=PASS
main: lower_95_ratio=2.250345 speedup_percent=127.097 evidence=screen_positive
pathological: lower_95_ratio=134341.752434 speedup_percent=13872143.451 evidence=screen_positive
Decision: promoted (main_95>1=true pathological_95>1=true)

## candidate: prefilter-heldout (prefilter)
scope: held_out_confirmation
baseline_impl: naive
hypothesis: Pre-registered confirmation of the main+pathological win with seeds/order-seed not used in exploration.
commit:   vcpus: 4
PLAN: measure=scan main(count=8377 sessions=30 blocks=8) pathological(count=150 sessions=3 blocks=6) order_seed=77 seeds=[911]
gate: main=PASS pathological=PASS
main: lower_95_ratio=2.211432 speedup_percent=127.846 evidence=candidate_faster
pathological: lower_95_ratio=125165.828459 speedup_percent=13148460.710 evidence=candidate_faster
Decision: promoted (main_95>1=true pathological_95>1=true)
