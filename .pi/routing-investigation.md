# Live Writers Routing Investigation

## Summary

ROADMAP item 9 asks whether `kern ingest` and `kern link` should route writes to a central daemon when one is running, instead of always writing locally. After investigation, I recommend **keeping the status quo** (local writes with guarded flush) rather than routing to the daemon.

## Current Routing Mechanism

### Commands That Route to Daemon
These commands use the `route()` function to send operations to the daemon when available:
- `kern get` → `query` tool
- `kern query` → `query` tool  
- `kern forget` → `forget` tool
- `kern degrade` → `degrade` tool
- `kern promote` → `promote` tool
- `kern graviton add/remove` → `graviton` tool
- `kern claim-kind add/rm` → `claim_kind` tool
- `kern intake drain` → `intake_drain` tool

### Commands That Write Locally
- `kern ingest` → uses local graph loading + `flush_guarded()`
- `kern link` → uses local graph loading + `save_graph_guarded()`

## What Would Change If ingest/link Routed to Daemon?

### The Trust Problem
The main technical barrier is **trust level handling**:

1. **CLI `kern ingest`** creates Facts with confidence 1.0 via `clamp_confidence(1.0, "user")`
2. **MCP `tool_ingest`** clamps to `MAX_AI_CONFIDENCE` (0.95) via `clamp_confidence(p.conf, AGENT_SOURCE)`  
3. **CLI `kern link`** creates edges with confidence 1.0
4. **MCP `tool_link`** clamps to `MAX_AI_CONFIDENCE` (0.95)

If CLI commands routed through the daemon's MCP tools, **user-authored content would be silently demoted from Facts to Claims**, losing the trust signal that indicates human authorship.

### Alternative: Add Trust Field to Socket
The document mentions this approach was considered but rejected because:
- Shared secrets prove UID, not specific programs
- Multiple processes (CLI, hub, `kern mcp` proxy) run as same user  
- No way to cryptographically distinguish which program is calling
- Item 18 removal decision: kern carries no caller identity by design

## Benefits and Costs

### Benefits of Routing
- **Consistency**: All writes go through single daemon
- **Live graph**: Writes appear immediately in daemon's memory
- **No write conflicts**: Eliminates potential for competing writers

### Costs of Routing  
- **Silent trust demotion**: User Facts become agent Claims (0.95 vs 1.0 confidence)
- **Loss of human signal**: Facts indicate human authorship for ranking/belief model
- **Breaking semantic contract**: `kern ingest` would no longer create true Facts

## Current Safety Mechanisms

Both commands already use **guarded flush patterns** that prevent data corruption:

### `cmd_ingest`
```rust
let flushed = graph::persist::flush_guarded(&g.read(), expected);
match flushed {
    Ok(FlushOutcome::Flushed { .. }) => break,
    Ok(FlushOutcome::RefusedStale { .. }) => {
        // Reload graph and retry
        let fresh = crate::reload_graph(cfg, &w);
        *w = fresh;
        outcome = run_once(&worker, &g, &text, &src, kind, conf, cfg, valid_until).await;
    }
    // ... error handling
}
```

### `cmd_link` 
```rust
fn link_and_persist() -> Result<(String, f64), String> {
    let linked = link_entities(&mut g, from, to, reason_text, reason_embed, 1.0)?;
    let g = std::sync::Arc::new(parking_lot::RwLock::new(g));
    crate::save_graph_guarded(&g, cfg);  // Guarded flush
    Ok(linked)
}
```

These mechanisms ensure:
- **No data loss**: Failed flush triggers reload and retry
- **No corruption**: Refuses stale writes, never overwrites newer data  
- **Clean error reporting**: User sees clear message about write conflicts

## Recommendation: Keep Local Writes

**Recommended approach**: Maintain the status quo of local writes with guarded flush.

### Reasoning

1. **Trust preservation**: User-authored content maintains Fact status with confidence 1.0
2. **Semantic correctness**: Maintains distinction between human and agent contributions
3. **Safety maintained**: Guarded flush already prevents data corruption
4. **Design consistency**: Aligns with item 18's "no caller identity" decision
5. **Proven reliability**: Current system handles write conflicts cleanly

### Edge Cases Addressed

The document notes both commands are "one-shot, so the exposure is a lost write rather than a lost graph." The guarded flush mechanisms already handle this by:
- Detecting write conflicts via epoch checking
- Reloading latest state on conflict
- Retrying the operation with fresh data
- Clear error messages after retry limit

## Conclusion

The routing decision reveals a fundamental design choice: preserve semantic meaning (human vs agent authorship) or achieve write consistency through central daemon. Given kern's design principles around trust modeling and the existing safety mechanisms, **keeping local writes with guarded flush preserves more value than routing would add**.

The trust demotion cost outweighs the consistency benefits, especially since the guarded flush pattern already prevents the data safety issues that routing would solve.