# RFC-016 Round 2 — Author response

L: ACCEPT. K and M: narrow dissents, all conceded.

| Dissent | Disposition | RFC change |
| --- | --- | --- |
| K.D6 batch per-id disposition + Lua error handling | CONCEDE | §4.1 dispatch contract paragraph |
| K.D7 / M.D7 reconciler scan cost → per-partition index SET | CONCEDE, option (a) | §8.5 item 2 + new §6.3 key + §11 Stage C population/drain |
| M.D6 dispatcher-side coalescing across groups | CONCEDE | §4.1 coalescing paragraph + §11 Stage C benchmark extension |

All round-2 dissents are specification additions — no design changes from round 1.

Revisions applied in `rfc(016): round-2 revisions`.
