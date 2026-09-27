# Plan: Support "System One" decision-model backends (Laya / CLM / Kev / Jev)

Status: **implemented through P1, P2a, P2b and P4 (docs)** — `8972bcc`, `a5bdd44`, `792cf88`
and the docs commit. What was deliberately left out is listed in **§9 (Deferred, not
forgotten)** — deferred, not forgotten. Scope: attach decision-model servers **that the user
runs locally, directly** — `laya-serve`, `clm-serve`, `kev.serve` — as ollamaMQ backends.
Ollaya is explicitly **out of scope** (see §7).

All runtime facts below are read from the servers' **source code**, not their prose, on 2026-09-27:
`NandhaKishorM/laya@main laya/serve.py`, `Contrastive-LM/CLM@main src/clm/server.py`,
`jaredpalmer/kev@main kev/serve.py`, `docs.typesafe.ai/api.md`.
All internal facts are quoted from this repo as checked out.

---

## 0. Direct answer: "they all have the same endpoints, right?"

**No — only the one that matters is shared.** `POST /v1/systemone` is identical across all of them
(same request, same `answers`/`usage` response). Everything else differs, and **none of them
implement a single `/api/*` route**, which is where ollamaMQ's model listing and load/unload live.

Route matrix, from the `@app.get` / `@app.post` decorators in each server:

| Route | Laya<br>`laya/serve.py` | CLM<br>`src/clm/server.py` | Kev<br>`kev/serve.py` | TypeSafe (hosted Jev) |
|---|---|---|---|---|
| `POST /v1/systemone` | ✅ | ✅ | ✅ | ✅ |
| `GET /v1/models` | ❌ **absent** | ✅ `{"models":[…]}` | ✅ `{"models":[{"name",…}]}` | ✅ |
| `GET /health` | ✅ | ✅ `{ok, embedder, models, cache}` | ❌ **absent** | ❌ |
| `GET /` | ❌ | ✅ playground UI | ❌ | ❌ |
| `POST /v1/rank` | ❌ | ✅ | ❌ | ❌ |
| `POST /v1/systemone/permute` | ❌ | ❌ | ✅ | ❌ |
| `POST /v1/systemone/separate` | ❌ | ❌ | ✅ | ❌ |
| **any `/api/*`** | ❌ | ❌ | ❌ | ❌ |

Defaults and auth:

| | Bind default | Port | Key env var | Auth header |
|---|---|---|---|---|
| **Laya** | `0.0.0.0` ⚠️ | `LAYA_PORT` = **8000** | `LAYA_API_KEY` | `Authorization: Bearer` |
| **CLM** | `0.0.0.0` ⚠️ | `CLM_PORT` / `--port` = **8700** | `CLM_API_KEY` | `Authorization: Bearer` |
| **Kev** | `127.0.0.1` | `--port` = **8008** (README examples pass 8009) | `KEV_API_KEY` | `Authorization: Bearer` |

⚠️ Laya and CLM bind all interfaces **and are open by default** — anyone on the LAN can run
inference. Point ollamaMQ at a loopback bind, or set their key.

Good news: all three use `Authorization: Bearer`, which is exactly what ollamaMQ's existing
`Backend { url, token }` (`config.rs:98-110` → `attach_backend_token`, `dispatcher.rs:250-263`)
already emits. **No config-schema change is needed for auth.**

### 0.1 The shared payload (identical on all four)

```json
{ "state": "billed twice, refund please or we cancel",
  "model": "clm-latest",
  "questions": {
    "dept":    { "type": "choice", "instructions": "which team?",
                 "criteria": { "billing": "refunds", "tech": "bugs" } },
    "urgency": { "type": "score",  "criteria": ["Can wait", "Today"] },
    "refund":  { "type": "noul",   "instructions": "Is a refund justified?" } } }
```

Response: `model`, `answers` (same keys), `usage.{input_tokens,output_tokens}`.
`choice` → `{type, choice, probabilities, confidence}`; `score` → `{type, score, legend,
probabilities, confidence}`; `noul` → `{type, noul}`.

Per-runtime payload quirks worth knowing (they pass through byte-transparently, but they will show
up in errors): Laya caps choice options at 100 (413) and its `confidence` is 1 − normalised entropy,
**not** Jev's formula; `model` is honoured only for `english`/`multilingual`/`typed-decisions`,
otherwise the router auto-selects. CLM adds `temperature` and `billing_units`, and 502s when its
vLLM embedder is down. Kev answers within a 16,384-token row budget per pass.

---

## 1. Consequences — this is simpler than the Ollaya design, in three ways

1. **There is no model-control surface to integrate.** No `/api/ps`, no `keep_alive`, no
   load/unload call. Residency is each runtime's own business (`LAYA_MAX_LOADED`, CLM's vector
   arena, Kev's single loaded run). ollamaMQ's `execute_load`/`execute_unload`
   (`control.rs:869-937`, `control.rs:985+`) simply **do not apply** to this family, and
   `supports_control` (`control.rs:589-591`) must keep returning false for it. That deletes a whole
   phase from the earlier plan.
2. **The inference path needs no translation at all** — it is one route, byte-transparent, exactly
   what `proxy_handler` already does.
3. **CLM's and Kev's extra routes are just more routes.** `/v1/rank`, `/permute`, `/separate` are
   ordinary pass-through once the family exists.

## 2. What breaks today (file:line)

1. **Prefix-based family detection misroutes every request.** `detect_api_family`
   (`dispatcher.rs:139-147`) maps `/v1/*` → `OpenAi`. So `/v1/systemone` is routed to LM Studio /
   vLLM backends and 404s there; `/v1/rank` likewise.
2. **No third family exists.** `BackendApiType { Unknown, Ollama, OpenAi, Both }`
   (`dispatcher.rs:83-89`), `ApiFamily { Ollama, OpenAi, Unknown }` (`dispatcher.rs:123-128`),
   `family_compatible` (`dispatcher.rs:818-824`).
3. **Probe would mark them offline or mislabel them.** `probe_backend` (`control.rs:156-372`) tries
   `/api/tags`, `/api/ps`, `/v1/models` (expecting OpenAI's `{"data":[{"id"}]}`), `/api/v1/models`,
   then falls back to `GET /`. Against Laya every one of those 404s → backend looks **offline**.
   Against CLM, `/v1/models` returns `{"models":[…]}` → parsed as OpenAI, `data` missing, no merge.
   Against Kev, `/v1/models` returns `{"models":[…]}` → same.
4. **A 404 never teaches anything.** `is_endpoint_rejection` (`dispatcher.rs:830-854`) recognises
   405 or 200-with-error-JSON only, so a mis-routed `/v1/systemone` is retried until
   `stuck_timeout` → 503 instead of being reclassified.
5. **`/v1/models` aggregation drops these backends.** `model_list_shape`
   (`dispatcher.rs:150-156`) knows only `("api/tags","models","name")` and
   `("/v1/models","data","id")`; `aggregate_model_list` skips any backend whose array key is not an
   array (`dispatcher.rs:212-216`).
6. **Laya cannot be model-listed at all, and unknown names are fatal.** `laya/serve.py` has no
   `/v1/models`. The scheduler filters requests for models not in `available_models`
   (`dispatcher.rs:1307-1318`) and 503s after `stuck_timeout` (`dispatcher.rs:1385-1470`). So a
   Laya backend would reject **every** request unless its checkpoints are declared in
   `appconf.yaml`. See §3.4 — this is the main gotcha of direct-attach.

---

## 3. Design

**3.1 One new family, route-table-driven.** Add `ApiFamily::SystemOne` / `BackendApiType::SystemOne`
and replace `detect_api_family(path)` with `families_for_route(method, path)`:

| Route | Family | Notes |
|---|---|---|
| `/v1/systemone`, `/v1/decisions` | `{SystemOne}` | `/v1/decisions` is an alias some clients use; harmless to accept |
| `/v1/systemone/permute`, `/v1/systemone/separate` | `{SystemOne}` | Kev extras |
| `/v1/rank` | `{SystemOne}` | CLM extra |
| `/v1/models`, `/v1/models/{model}` | `{OpenAi, SystemOne}` | shared name, different envelope — see 3.3 |
| `/health` | **not a proxy route** | ollamaMQ already owns `/health` as its own liveness endpoint, registered outside the auth layer (`main.rs:380-383`). It is used **only** by the probe (3.2). A client's `GET /health` must keep answering "is ollamaMQ alive" and never reach a backend. |
| `/api/*`, `/v1/chat/completions`, `/v1/completions`, `/v1/embeddings` | unchanged | a SystemOne backend must never receive these |

**Implemented in P1 (`8972bcc`), with one clarification.** The table is live except for the
`/v1/decisions` alias and the CLM/Kev extra routes (dropped by decision §8.2, see §9):
`families_for_route(method, path)` replaced the prefix rule, `/v1/systemone` is registered as a
proxy route, and a test re-derives every registered route against the old rule so no pre-existing
route changed family. The `/api/*` row is enforced in the strongest sense — a SystemOne backend is
never even *asked* for `/api/tags` — yet its model **names** are still listed on that route by the
proxy itself. That is decision (a) in §3.3, and it is deliberate.

Keep `BackendApiType` as a bitset (`ApiSet(u8)`) rather than adding variants — `Ollama|OpenAi|Both`
already exhausts the current lattice, and `apply_probe` (`control.rs:376-397`) replaces `api_type`
wholesale each round anyway.

**3.2 Health probe: per-runtime fallback, because no route is universal except `/v1/systemone`.**
Order for a SystemOne candidate:
1. `GET /v1/models` → if the JSON has a top-level **`models` array with `name` fields**, it is
   SystemOne and online (CLM, Kev, hosted Jev). Distinguishes from OpenAI's `data`/`id`, so it is
   unambiguous.
2. else `GET /health` → 2xx JSON ⇒ online, **but do not set the OpenAI type** (this is Laya).
   `/health` stays internal to the probe; see the route table in 3.1 — it must not be added to the
   proxy route table, because `main.rs:380-383` already serves `/health` itself.
3. else optional authenticated `POST /v1/systemone` liveness probe with a 1-question body —
   the only route guaranteed to exist. Gate this behind config; it costs a forward pass.
When SystemOne is detected, **suppress** the `/api/tags` → `Ollama` merge for that backend.
Also treat **404 as a family rejection** in `is_endpoint_rejection` when the backend was not
positively detected for that family, so mis-tags self-correct.

*Shipped: step 1 (`8972bcc`), step 2 and the suppression (`a5bdd44`); `/health` stayed
probe-internal and out of the proxy route table, as required above. Deferred: step 3 (the
inference-costing liveness probe) and the 404-as-rejection rule — §9.*

**3.3 Listing aggregation must accept both envelopes — but answer in ONE.** For `/v1/models`, read
`data`/`id` **or** `models`/`name`, normalise, dedup on name. **The response envelope must be
route-determined, never first-answerer-determined**: `/v1/models` always answers in OpenAI's
`data`/`id`, because README:112 advertises that route as OpenAI-compatible and OpenAI SDKs require
`data`. Accepting `models`/`name` on the way in must not let a SystemOne backend ordered first
re-key the merged list (this was a real regression introduced in P1 and caught in review).
`/api/tags` keeps `models`/`name`.

Two decisions were taken while implementing this and are now intentional, tested behaviour —
**do not "fix" them**:

- **(a) Decision-model names are surfaced in BOTH aggregated listing routes**, `GET /v1/models`
  **and** `GET /api/tags`, even though only `POST /v1/systemone` can serve them. Deliberate:
  ollamaMQ aggregates **one namespace** and routes by capability, so a listing advertises
  everything the proxy can route to, and a request a backend cannot serve is **refused, not
  misrouted**. A SystemOne backend is never contacted on `/api/tags` (`/api/*` must not reach it)
  — the proxy contributes that backend's known names from `available_models`, appended after the
  live entries and deduped through the existing seen set, so a live entry's
  `size`/`digest`/`description` always wins whatever the backend order. Names for a backend that
  cannot list anything are synthesised in the route's own envelope and sorted, so the listing does
  not shuffle between runs (`792cf88`). Before that, a Laya-only deployment answered
  `GET /v1/models` with Laya's own 404 and left `GET /api/tags` hanging until `stuck_timeout`.
- **(b) `/v1/models` answers in OpenAI's `data`/`id` unconditionally**, whichever envelope the
  backends actually replied in and whichever of them answered first. `models`/`name` is read on
  input only and re-keyed on the way out, and the first answerer's foreign array key is dropped so
  the reply is one envelope, not two. `/api/tags` symmetrically always answers `models`/`name`.

**3.4 Model names must be admitted, and config alone does NOT do it.** ⚠️ **Corrected after P1.**
The original text here claimed declaring models in `appconf.yaml` would make them routable. That is
**false**, and was verified false against a live CLM on 2026-09-27:

| Probe through the proxy | Result |
|---|---|
| `GET /admin/models` | `api: "???"`, `available_models: []`, `online: true` |
| `GET /v1/models` | 200 — `clm-latest`, `clm-raw` (the *merge* path sees them) |
| `POST /v1/systemone` **without** `model` | 200 in 31 ms, byte-identical incl. `meta` |
| `POST /v1/systemone` **with** `"model":"clm-latest"` | **503 after 60 s** (`stuck_timeout`) |

Why: `available_models` has exactly **one** production writer, `apply_probe` (`control.rs:379`), fed
only from `/api/tags → models[].name` (`control.rs:196`) and `/v1/models → data[].id`
(`control.rs:291`). A `models:` entry never writes it — `model_pin_for` (`dispatcher.rs:832`)
only *restricts* candidates and changes the 503 wording, and `apply_model_config` is rejected by
`supports_control` (`control.rs:589`). So a name that the probe cannot discover is unroutable, full
stop. Two consequences for P2:

1. **CLM/Kev need no config at all** — teach the probe to read `/v1/models`' `models[].name` shape
   and populate `available_models` from it. That alone makes `clm-latest` routable.
2. **Laya genuinely needs declared names**, because it exposes no listing endpoint — and P2 must
   *union* declared names into `available_models` (in `apply_probe`, so `/admin/models` and the TUI
   show them too). Do this by reusing the existing `config_targets` selector (`control.rs:596`).

Config shape stays as below; it is only *effective* once P2 item 2 lands:

```yaml
backends:
  - url: http://127.0.0.1:8700          # clm-serve
    token: "clm-key"                    # CLM_API_KEY -> Authorization: Bearer
  - url: http://127.0.0.1:8000          # laya-serve (LAYA_API_KEY set, LAYA_HOST=127.0.0.1)
    token: "laya-key"
  - url: http://127.0.0.1:8008          # kev.serve

models:
  - name: english                       # Laya checkpoints: not discoverable, must be declared
    backends: ["http://127.0.0.1:8000"]
  - name: multilingual
    backends: ["http://127.0.0.1:8000"]
  - name: typed-decisions
    backends: ["http://127.0.0.1:8000"]
  - name: clm-latest
    backends: ["http://127.0.0.1:8700"]
```

Optional shortcut: an explicit `kind: systemone` on a backend skips probing entirely — worth having
precisely because Laya is hard to detect.

**3.5 Model control: unsupported, and that must be visible.** `/admin/models/load` for a SystemOne
backend should return a clear 400 ("backend has no model-control API; load is implicit") rather than
the current generic unsupported message, since these servers preload via env vars, not HTTP.

*The behaviour is right and the wording is not: `supports_control` is a containment test on the
`Ollama` bit, so a SystemOne-only backend is refused while an `Ollama|SystemOne` backend keeps its
load path. The clearer message is deferred (§9). What did land in this area is §3.4's mechanism: a
`models:` entry makes a name **routable** on its selected backends — unioned into
`available_models` by `apply_probe` through the existing load selector — which is what makes a
listing-less Laya usable at all (`a5bdd44`).*

**3.6 Not doing:** translating between Ollama/OpenAI and System One. Different semantics, not
different encodings — a `/api/chat` request has no honest rendering into typed questions. Route
away, as the codebase already does.

---

## 4. Phases

**P1 — Family + route table + dual-envelope listing.** `ApiSet`, `families_for_route`,
`family_compatible`, `/v1/models` accepting `models`/`name`. Unit tests in `src/dispatcher.rs`
(near `dispatcher.rs:2091+`): exhaustive route-table matrix, and a merge across one `data`/`id` and
one `models`/`name` backend.
*Done when:* `cargo test` green; no existing route's classification changed.

> **DONE — `8972bcc`** (92 tests green). Also landed here: `/v1/systemone` registered as a proxy
> route, `supports_control` switched to a containment test, and the startup readiness wait no
> longer stalls on a decision-model-only setup. `BackendApiType` became an `ApiSet` bitset as
> planned. `/v1/decisions`, `/v1/rank` and the Kev extras were dropped by decision §8.2 (§9).

**P2a — Probe discrimination, family pin, declared names.** *(was "P2 — Probe discrimination";
nits found in live testing split it in two)* SystemOne detection via `/v1/models`
(`models`+`name`), `/health` fallback, suppression of the false `Ollama` merge, 404-as-rejection.
Extend the mock routers (`control.rs:1719-1762`) with all three real shapes.
*Done when:* three mocks — CLM (`/v1/models`+`/health`), Kev (`/v1/models` only), Laya (`/health`
only) — are each detected online and SystemOne-only, and `/api/chat` is never routed to any of them.

> **DONE — `a5bdd44`** (101 tests green), except 404-as-rejection (§9). Landed: the 2xx
> `GET /health` fallback (any JSON body — Laya answers `{"status":"ok"}`, CLM `{"ok":true}`), the
> per-backend `kind: systemone` pin that survives `apply_probe` replacing the detected set each
> round, and `available_models` = **discovered ∪ declared**, admitted in `apply_probe` via the
> existing selector so a name that cannot be discovered is still routed and still shows in
> `/admin/models` and the TUI. The three real shapes exist as unit-test mocks in `control.rs`; the
> e2e mocks still have no System One server (§9). Verified against a live `laya.serve` and a live
> `clm-serve` in one pool: a decision naming `clm-latest` reached CLM, one naming `multilingual`
> reached Laya.

**P2b — Listings from backend state.** *(not in the original plan; found by running it)* A backend
that does not implement a listing route contributed nothing to the merge, and when no backend
answered, `aggregate_model_list` returned `None` and let the request fall through to that backend's
own answer — so a Laya-only deployment served `GET /v1/models` → 404 and hung `GET /api/tags` until
`stuck_timeout`, even though routing its models worked.
*Done when:* both listing routes answer 200 with the known names on a listing-less backend, and a
mixed pool lists every name once with the live backend's metadata intact.

> **DONE — `792cf88`** (105 tests green). Backends that return no parseable listing contribute
> their `available_models` as minimal entries in the route's own envelope, after the live entries,
> and a 200 envelope is returned when nothing answered but names are known. Ollama / LM Studio /
> OpenAI listings are provably unchanged: a backend incompatible with a route still contributes
> nothing unless it is System One. Live: Laya-only returns 200 on both routes; a mixed CLM+Laya
> pool listed `clm-latest`, `clm-raw` and `multilingual` once each with CLM's
> `description`/`release_date` intact.

**P3 — Extra routes + config.** `/v1/rank`, `/v1/systemone/permute`, `/v1/systemone/separate`,
`/v1/decisions`; optional `kind: systemone`; explicit clear 400 on load/unload for this family.
*Done when:* e2e asserts each extra route reaches only SystemOne backends.

> **PARTIAL — `a5bdd44` shipped `kind: systemone`**, plus §3.4's declared-name unioning, which was
> the part that actually unblocked Laya. The extra routes are excluded by decision §8.2 and the
> clearer `/admin/models/load` message is deferred — both in §9.

**P4 — Docs.** README "Supported Endpoints" (`README.md:89-111`) and model-control section
(`README.md:127-136`), an `appconf.yaml.example` block like §3.4, and the explicit warning that
**Laya and CLM bind `0.0.0.0` open by default** and that Laya's models must be declared by hand.
*Done when:* README documents the family and the config example is copy-pasteable.

> **DONE — docs commit** (this change; no code touched). README gained a `POST /v1/systemone` entry
> in "Supported Endpoints", a "Decision-model backends (System One)" subsection (routes proxied,
> detection and `kind: systemone`, declared-name routability, the aggregated-listing decision, the
> loopback-bind warning, CLM's vLLM encoder dependency) and a System One bullet in the
> model-control backend-support list. `appconf.yaml.example` gained the commented CLM/Laya/Kev
> block with one comment per gotcha. Verified at this commit: `cargo test` 105 passed,
> `tests/e2e/run.sh` 20 passed.

First commit should be **P1 alone** — self-contained, cannot regress existing traffic.

## 5. Test plan

- **Unit** (`cargo test`): route matrix; `ApiSet` merge/`supports`; probe classification for the four
  real shapes (Ollama `models`/`name` on `/api/tags`, OpenAI `data`/`id`, SystemOne `models`/`name`
  on `/v1/models`, Laya `/health`-only); 404-as-rejection learning.
- **E2E** (`tests/e2e/run.sh` + `tests/e2e/mock_backends.py`): add mocks mirroring each server's
  real route set. Assert `/v1/systemone` reaches only SystemOne backends, `/api/chat` never does,
  `/v1/models` merges both envelopes, and load/unload returns the clear 400.
- **Smoke** (manual): run one real server on loopback, one `curl` through ollamaMQ with the §0.1
  body, expect `answers` back byte-identically.

## 6. Risks

| Risk | Mitigation |
|---|---|
| Laya backend looks offline (no probe route matches) | §3.2 `/health` fallback + optional `kind:` |
| Laya rejects every request (undeclared model names) | §3.4 declare in `appconf.yaml`; documented loudly |
| Tagging one of these as Ollama/OpenAI → `/api/chat` storms | §3.2 positive discrimination + 404-as-rejection |
| Rewriting `detect_api_family` regresses Ollama/LM Studio | P1 test enumerates the current route list (`main.rs:333-382`) before adding anything |
| Laya/CLM bind `0.0.0.0` open by default | P4 warning; recommend loopback bind or their key |
| CLM 502s when its vLLM embedder (:8090) is down — looks like a dead backend | pass the 502 through; do not mark offline on a 502 |
| Kev's first request after Modal idle takes ~35 s | exceeds nothing by default (`timeout` 300 s) but worth noting in docs |

## 7. Out of scope

- **Ollaya.** Excluded by decision. Note what that costs: no `/api/decide`, no `/api/tags`,
  no `/api/ps`, no `keep_alive`-based load/unload, no single multi-model upstream. If that control
  plane is ever wanted, it is additive to this plan — §3.1's route table just gains rows.
- Translating between API families.
- Streaming decisions (`/v1/systemone` is request/response only).
- Serving or managing weights from ollamaMQ.
- Per-request `usage`/`billing_units` accounting.

## 8. Decisions taken

1. **`cml` = CLM (Contrastive-LM). Confirmed by Lukas.**
2. **Core routes only.** `/v1/rank`, `/v1/systemone/permute` and `/v1/systemone/separate` are NOT
   proxied. P3 shrinks to the optional `kind: systemone` shortcut and the clearer 400 on
   `/admin/models/load`.
3. **Live validation available.** A real `clm-serve` runs on `192.168.1.79:8700` (encoder on `:8090`).
   Verified against it on 2026-09-27, and it matches `src/clm/server.py` exactly:

   | Probe | Result |
   |---|---|
   | `GET /v1/models` | 200 `{"models":[{"name":"clm-latest","release_date","description"},{"name":"clm-raw",…}]}` |
   | `GET /health` | 200 `{"ok":true,"embedder":true,"models":[…],"device":"cuda","device_ok":true,…}` |
   | `GET /` | 200 playground HTML |
   | `GET /api/tags` | **404** `{"detail":"Not Found"}` — confirms the Ollama probe cannot see it |
   | `POST /v1/systemone` | 200 in **47 ms** |

   **The response is a superset of TypeSafe's spec**: alongside `model`/`answers`/`usage` it returns
   `usage.billing_units` and a whole `meta` object (`device`, `api_ms`, `temperature`,
   `logit_scale`, `calibrated`, …). Byte-transparent pass-through forwards this untouched — a
   concrete reason not to parse or rebuild the body on the proxy path.
2. Which of these do you actually run? P2's mocks can be replaced by a live server for validation,
   which is worth much more than a mock.
3. Do you want the Kev/CLM extra routes (`/v1/rank`, `/permute`, `/separate`) proxied at all, or
   only `/v1/systemone`? Dropping them shrinks P3 to almost nothing.
4. Hosted Jev (`api.typesafe.ai`) — in or out? It is HTTPS, keyed, and its `/v1/models` is not a
   local model list; it likely wants `kind: systemone` forced rather than probed.

---

## 9. Deferred, not forgotten

Everything below was specified above and then deliberately left unimplemented. None of it is a bug
waiting to be rediscovered; each row is a scope decision with its reason.

| Item | Specified in | Status and why |
|---|---|---|
| **404 as a family rejection** | §3.2, last line | Not implemented. `is_endpoint_rejection` still recognises only 405 and 200-with-error-JSON, so a mis-tagged backend is retried until `stuck_timeout` rather than reclassified. Deferred because P1+P2a remove the misrouting it would heal: `/v1/systemone` is looked up in the route table instead of prefix-guessed, and a `/v1/models` listing, a 2xx `/health` or a `kind:` pin classifies the backend positively. Loosening a *general* rejection predicate to "404 means wrong family" would also let a transient 404 teach the proxy something false — that deserves its own change, not a bolt-on. |
| **FastAPI `{"detail":"Not Found"}` recognition** | §8.3 (`GET /api/tags` → 404 on a live CLM) | Not implemented. It is the JSON half of the same predicate above; they should land together. |
| **Clear 400 on `/admin/models/load` for this family** | §3.5 | Not implemented — the generic unsupported-backend message is returned. Behaviour is already correct (SystemOne-only backends refuse control; `Ollama|SystemOne` keeps it), so this is wording only. |
| **`/v1/rank`, `/v1/systemone/permute`, `/v1/systemone/separate`, `/v1/decisions`** | §3.1 rows, P3 | Not routed, per decision §8.2 (core routes only). Revisiting that decision means adding rows to §3.1's table plus pass-through tests; the family itself needs nothing else. |
| **Optional authenticated `POST /v1/systemone` liveness probe** | §3.2 step 3 | Not implemented, and currently unnecessary: 2xx `/health` plus `kind: systemone` cover the only undetectable runtime (Laya). A probe that costs a forward pass every 10 s must not ship without the config gate §3.2 already demands. |
| **e2e coverage of the family** | §5 | `tests/e2e/mock_backends.py` still ships only `OllamaMock` and `LMMock`; the 20 e2e checks never touch `/v1/systemone`. The three real route surfaces are covered as unit mocks in `control.rs`/`dispatcher.rs` and by the live `clm-serve` / `laya.serve` runs recorded in the P1/P2a/P2b commit messages. |
