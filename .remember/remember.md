# Handoff

## State
Module migration complete. `src/valkey/` deleted. All cluster files in `src/cluster/`, connection in `src/connection/`, protocol in `src/protocol/`. Core types flattened: `src/cmd.rs`, `src/pipeline.rs`, `src/value.rs`, `src/retry_strategies.rs`, `src/macros.rs`. PubSub in `src/pubsub/`.

267 lib tests pass, 0 clippy errors, 0 warnings.

### Benchmark baseline (full profile, 60s runs, 3-run median, ElastiCache 3-shard TLS, 2026-04-12)
4 complete runs. Run 1 was cold-start (depressed ~25%). Runs 2-4 are warmed and consistent.
Canonical baseline = median of runs 2-4:

| Permutation | Run1(cold) | Run2 | Run3 | Run4 | **Baseline** |
|---|---|---|---|---|---|
| cluster-100B-c100 | 261K | 353K | 329K | 352K | **352K** |
| cluster-1KB-c100  | 233K | 302K | 293K | 307K | **302K** |
| cluster-16KB-c100 |  94K | 106K |  90K |  99K |  **99K** |
| mget-10keys-c100  | 284K | 308K | 308K | 308K | **308K** |
| pipeline-c100     | 186K | 189K | 184K | 187K | **187K** |
| batch-xslot-c100  |  89K |  90K |  88K |  88K |  **89K** |

p50 at peak (1KB-c100): ~0.26ms. p99: ~1.17ms.
No regression vs prior 264K quick-profile baseline (which was also warm-cache).
DESIGN_DEBT #7 (Cmd double-alloc) unlikely to move these numbers significantly per research.
Results files: bench-results-full-1775976379.json (run1), bench-full-run{2,3,4}.log

### Completed DESIGN_DEBT items
- **#1 Module structure**: `src/valkey/` flattened and deleted. `valkey/mod.rs` was the last file, now gone.
- **#3 Convenience API**: `ferriskey::Client` with `get/set/del/incr/expire/exists/hset/hget/hgetall/lpush/rpop/get_set/set_ex`. Typed `Pipeline` with `PipeSlot<T>`. `Client::transaction()` for MULTI/EXEC.
- **#4 PubSub rewrite**: DONE. `EventDrivenSynchronizer` replaces `ValkeyPubSubSynchronizer`. Event-driven via mpsc channel (no 3s polling). Single `ConfirmedState`, no `pending_unsubscribes` queue. Old code backed up as `synchronizer_v1.rs`. Standalone + rapid subscribe/unsubscribe test gaps CLOSED (3 new tests in `tests/test_pubsub.rs::standalone_pubsub_tests`).
- **#8 Arc\<str\> for addresses**: DONE. `DashMap<Arc<str>, ClusterNode>` in container.rs. Connection lookups are refcount bump.
- **#9 Error consolidation**: FULLY COMPLETE. `ConnectionError`, `StandaloneClientConnectionError`, `IAMError` collapsed into `ValkeyError`. `ServerError`/`ServerErrorKind` deleted — `Value::ServerError` now wraps `ValkeyError` directly. Single error hierarchy.
- **DefaultHasher non-determinism**: DONE. FNV-1a deterministic hasher in topology.rs.
- **assert_eq! in hot path**: DONE. Converted to `debug_assert_eq!` in routing.rs.
- **Box::leak fix**: DONE. `new_with_response_timeout` takes owned `ConnectionInfo`.

### Public API surface
Flat re-exports from `ferriskey::` root: `Cmd`, `cmd`, `Value`, `ErrorKind`, `ValkeyError`, `ValkeyResult`, `FromValkeyValue`/`FromValue`, `ToValkeyArgs`/`ToArgs`, `ConnectionAddr`, `ConnectionInfo`, `ValkeyConnectionInfo`, `ProtocolVersion`, `PubSubSubscriptionKind`, `PushInfo`, `RetryStrategy`, `Client`, `ClientBuilder`, `CommandBuilder`, `TypedPipeline`, `PipeSlot`, `PipeCmdBuilder`, `ReadFrom`, `FerrisKeyError`.

Feature flags: default=[], test-util=[mock-pubsub].

## Next
1. **DESIGN_DEBT #6 — Value::ServerError removal**: Parser returns `Err(ValkeyError)` instead of `Ok(Value::ServerError(e))`. Now unblocked (#9 complete). Requires changing parser + pipeline result arrays from `Vec<Value>` to `Vec<Result<Value, ValkeyError>>`. 25 consumer sites across 6 files.
2. **DESIGN_DEBT #7 — Cmd double-allocation**: `Cmd` stores args in `data: Vec<u8>` + `args: Vec<Arg<usize>>`, then `get_packed_command()` re-serializes into RESP. Build directly in RESP format.
3. **DESIGN_DEBT #2 — Public API naming**: Rename `ValkeyError`→`Error`, `FromValkeyValue`→`FromValue`, `ToValkeyArgs`→`ToArgs` at crate root. Type aliases already exist.

## Context
- ElastiCache cluster: `clustercfg.glide-perf-test-cache-2026.nra7gl.use1.cache.amazonaws.com:6379` TLS, 3 shards, same VPC.
- ElastiCache standalone: `master.ferriskey-standalone-test.nra7gl.use1.cache.amazonaws.com:6379` TLS, SG sg-0c74e2e41027e3285 + inbound rule for sg-0171012222e7a6f61.
- Standalone PubSub tests: `VALKEY_STANDALONE_HOST=master.ferriskey-standalone-test.nra7gl.use1.cache.amazonaws.com VALKEY_TLS=true cargo test --test test_pubsub standalone_pubsub`
- Bench command: `VALKEY_HOST=... VALKEY_PORT=6379 VALKEY_TLS=true BENCH_PROFILE=short BENCH_RUNS=3 BENCH_CPUS=4-15 cargo bench --bench connections_benchmark`
- User is AWS employee (account 507286591552, role dev-x-oncall).
- User has 3 tmux workers (`team-msg worker-{1,2,3} manager "task"`). Use them aggressively.
- User wants zero tolerance: fix ALL findings, not just critical. No leftover naming, no dead code, no stale comments.
- 2 pre-existing flaky tests: `test_read_from_replica_az_affinity*` — mock HELLO handler bug, not our regression.
