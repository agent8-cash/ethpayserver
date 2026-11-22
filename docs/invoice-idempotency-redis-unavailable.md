# Invoice Idempotency: fail-closed when Redis is unavailable

**Decision:** When Redis is unavailable and a request with an `Idempotency-Key` header arrives for an invoice operation, return HTTP 503 Service Unavailable instead of proceeding with the request.

## Rationale

Invoice idempotency protects against duplicate charges when network failures occur. A client provides an `Idempotency-Key` header expecting exactly-once semantics: the same key + body will produce the same result, always.

When Redis is unavailable, the idempotency cache and in-flight lock cannot function. Proceeding with the request (fail-open) silently violates the idempotency contract:

1. **Concurrent requests** with the same key could both reach the handler
2. **Multiple invoices** could be created for the same logical payment
3. **Financial correctness is compromised** — merchants have no mechanism to deduplicate the invoices later

Rejecting the request (fail-closed) preserves correctness:

1. The client receives a clear error signal
2. The idempotency contract remains unbroken
3. The client can retry after the outage is resolved
4. Merchants avoid silent data corruption

Redis outages are infra-level events — rare, brief, and resolved by ops. Temporarily rejecting invoice creation is acceptable and preferable to double-charging.

## Implementation

**Any Redis operation failure in the idempotency middleware now returns 503.**

The middleware performs two checks on an idempotent request:

1. **Cache lookup** — Check if this (scope + key) has been seen before
   - Hit with same request hash → replay cached response
   - Hit with different hash → return 409 Conflict
   - Miss → proceed to lock acquisition
   - **Error → return 503 Service Unavailable** (changed)
2. **Lock acquisition** — Prevent concurrent requests with the same key
   - Lock acquired → proceed with request, cache response on success
   - Lock held by in-flight request → return 425 Too Early
   - **Error → return 503 Service Unavailable** (changed)

Previously, both error cases allowed the request to proceed unsafely. Now they reject with 503, preserving the idempotency contract.

## Impact

- **Boot-time outage:** `IdempotencyState::from_env` still returns immediately and connects in the background. The middleware passes through unchanged (same as before) until Redis answers.
- **Live connection failure:** If Redis connection drops after being established, invoice requests with idempotency keys now return 503 instead of proceeding unsafely.
- **Health signal:** The `ethpayserver_idempotency_healthy` gauge continues to track Redis health.

## Testing

Acceptance criteria:

1. When Redis is unavailable at boot, requests pass through unchanged until Redis is reachable
2. When a live Redis connection fails, idempotent invoice requests return 503
3. When lock acquisition succeeds, requests proceed normally
4. When a concurrent request holds the lock, return 425 Too Early (unchanged)
5. Non-idempotent requests (no `Idempotency-Key` header) pass through unchanged
