# Handoff

## State
All 381 tests pass (267 lib + 78 integration + 36 others). 0 errors. 0 warnings. Latest commit: 3c8f07f.

### Architecture
- `src/valkey/` deleted — modules now at `src/cluster/`, `src/connection/`, `src/protocol/`, `src/pubsub/`, `src/cmd.rs`, `src/pipeline.rs`, `src/value.rs`
- `src/ferriskey_client.rs` — public API: `Client`, `ClientBuilder`, `TypedPipeline`, `PipeSlot`, `connect()`, `connect_cluster()`
- `src/client/` — internal high-level client (ClientWrapper with routing logic)
- `src/pubsub/synchronizer.rs` — EventDrivenSynchronizer (event-driven, no polling)

### Performance baseline (full profile, warm cache, ElastiCache 3-shard TLS, 2026-04-12)
- cluster-100B-c100: 352K ops/sec, p50=0.21ms, p99=1.10ms
- cluster-1KB-c100:  302K ops/sec, p50=0.26ms, p99=1.17ms
- cluster-16KB-c100:  99K ops/sec, p50=0.97ms, p99=1.59ms
- mget-10keys-c100:  308K ops/sec, p50=0.24ms, p99=1.17ms
- pipeline-c100:      187K ops/sec, p50=0.43ms, p99=1.30ms
- bench results: bench-results-full-1775976379.json (run1, cold), bench-full-run{2,3,4}.log (warm)

### Completed DESIGN_DEBT
- #1 Module structure (src/valkey/ deleted)
- #3 Convenience API + TypedPipeline + connect/transaction
- #4 PubSub rewrite (EventDrivenSynchronizer, event-driven mpsc, no polling)
- #5 Box::leak eliminated (new_with_response_timeout takes owned ConnectionInfo)
- #8 Arc<str> node addresses
- #9 Error hierarchy (IAMError + StandaloneClientConnectionError + ConnectionError + ServerError + ServerErrorKind all eliminated → single ValkeyError)
- #10 Feature flags consolidated (default=[], test-util=[mock-pubsub])
- ValueCodec false EOF fix: decode() returns Ok(Some(Err)) not Err for server errors to prevent tokio-util Framed from setting has_errored=true

### Open DESIGN_DEBT
- #6 partial: parser returns Err(ValkeyError) for top-level errors; cluster/pipeline NodeResponse = ValkeyResult<Value>; Value::ServerError still needed for array elements (EXEC responses, RESP3 inline errors). Full removal requires changing Value::Array → Vec<ValkeyResult<Value>>.
- #11 ConnectionRequest visibility (eventually pub(crate))
- #7 Cmd double-allocation (deferred, bench first)
- #2 type renames ValkeyError→Error etc. (~700 occurrences, low priority)

### Infrastructure
- ElastiCache cluster: clustercfg.glide-perf-test-cache-2026.nra7gl.use1.cache.amazonaws.com:6379 TLS, 3 shards
- ElastiCache standalone: master.ferriskey-standalone-test.nra7gl.use1.cache.amazonaws.com:6379 TLS
  - SGs: sg-0c74e2e41027e3285 + inbound from sg-0171012222e7a6f61 (dev instance)
  - Standalone PubSub tests: VALKEY_STANDALONE_HOST=master.ferriskey-standalone-test... VALKEY_TLS=true cargo test --test test_pubsub standalone_pubsub
- Bench: VALKEY_HOST=clustercfg... VALKEY_PORT=6379 VALKEY_TLS=true BENCH_PROFILE=full BENCH_RUNS=3 BENCH_CPUS=4-15 cargo bench --bench connections_benchmark
- User: AWS employee. 3 tmux workers (team-msg worker-{1,2,3} manager "task"). Zero tolerance.
- 0 pre-existing flaky tests (all fixed this session).
- GitHub: github.com/avifenesh/FlowFabric, branch main

## Key Technical Notes
- StandaloneClient: Clone shares Arc<DropWrapper>. mark_as_dropped() is called by DropWrapper::drop() (when last Arc released), NOT by StandaloneClient::drop() (which fires on every clone drop).
- ValueCodec::decode() returns Ok(Some(Err(e))) for server errors (not Err(e)). tokio-util Framed sets has_errored=true when decoder returns Err, causing false None (fake EOF) on next poll.
- is_transaction in pipeline accumulator: true=transaction (Err→first_err, fails whole tx), false=non-tx (Err→buffer as ServerError, per-cmd error tracking).
- Value::ServerError still exists for array element errors (e.g., EXEC response with failed commands). parse_array() catches Err and stores as Value::ServerError(e).
