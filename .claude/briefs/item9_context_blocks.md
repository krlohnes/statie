# Unit 9: typed working-set blocks with pin / drop / recall (sketch, dispatch after item 2's arms are measured)

Only worth building if the model-owned arm is not clearly worse than the
current arm. Shape: each working-set window gets a stable id shown in the
harness view; `pin {id}` keeps it out of eviction, `drop {id}` evicts it now,
`recall {id}` restores an evicted window from the store. Harness eviction stays
the default. Measure as a fourth arm.
