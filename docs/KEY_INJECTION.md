# KEY_INJECTION — Bulk API Key Management (external automation)

Key injection is a no-UI API for scripts: bulk add / count / delete provider
API keys from outside the gateway. It lives under its own prefix —
`/admin/api/injection/*` — separate from the Auth Files page endpoints
(`/admin/api/keys/*`), which are used by the dashboard.

Base URL: `{BASE}/admin/api`

## Authentication

Admin token via `POST /admin/api/login`:

```json
{"username":"admin","password":"PASSWORD"}
```

`username` is optional unless the gateway has an admin username configured —
then it must match. Response contains a `token` field.

Use the token on every call: `Authorization: Bearer <token>`.

This document is also served live at `GET /admin/api/docs/key-injection`
(no auth).

---

## 1. Bulk Add Keys

**POST** `/admin/api/injection/bulk-add`

```bash
curl -X POST {BASE}/admin/api/injection/bulk-add \
  -H "Authorization: Bearer ***" \
  -H "Content-Type: application/json" \
  -d '{"provider_id":"sop","keys":["sk-xxx","sk-yyy"],"label":"optional"}'
```

**Response (success):**
```json
{"added":2,"duplicates":0,"total":2,"message":"2 added, 0 duplicates skipped"}
```

**Response (rejected):** HTTP 200 with an `error` field —
```json
{"added":0,"duplicates":0,"total":0,"message":"Provider 'xx' is not apikey (category: oauth)","error":"only apikey providers allowed"}
```

**Guards:**
- `keys` array must be non-empty (whitespace-only entries are dropped)
- `provider_id` must exist and be category `apikey` (oauth providers rejected)
- Duplicates (same provider + same `key_value`, non-oauth) are skipped and counted in `duplicates`
- All keys in one call share one `label` (optional; default empty)
- Provider is reloaded automatically after inserts, so new keys are usable immediately

---

## 2. Count Keys

**GET** `/admin/api/injection/count?provider_id=sop`

```bash
curl -H "Authorization: Bearer ***" \
  "{BASE}/admin/api/injection/count?provider_id=sop"
```

**Response:**
```json
{"provider_id":"sop","total":5,"active":5,"disabled":0}
```

Use this before a `disabled` delete to preview how many keys are candidates.

---

## 3. Delete Keys

**POST** `/admin/api/injection/bulk-delete`

### Delete only DISABLED keys (safe bulk)

Fetches all keys for the provider, keeps the active ones untouched, and
deletes only keys with `is_active = false`:

```bash
curl -X POST {BASE}/admin/api/injection/bulk-delete \
  -H "Authorization: Bearer ***" \
  -H "Content-Type: application/json" \
  -d '{"provider_id":"sop","action":"disabled"}'
```

**Response:**
```json
{"deleted":1,"total":2,"disabled":1,"kept_active":1,"message":"Deleted 1 disabled key(s) from sop; 1 active key(s) untouched"}
```

`total` = keys before the delete, `disabled` = candidates found,
`kept_active` = active keys left alone. When nothing is disabled:
```json
{"deleted":0,"total":1,"disabled":0,"kept_active":1,"message":"No disabled keys for 'sop' — all 1 key(s) are active, nothing deleted"}
```

### Delete one key by value

```bash
curl -X POST {BASE}/admin/api/injection/bulk-delete \
  -H "Authorization: Bearer ***" \
  -H "Content-Type: application/json" \
  -d '{"provider_id":"sop","action":"by_key","key_value":"sk-xxx"}'
```

**Response:**
```json
{"deleted":1,"message":"Deleted 1 key(s) from sop"}
```

**Guards:**
- `provider_id` must exist and be category `apikey`
- `action` must be `"disabled"` or `"by_key"` (with `key_value`)
- `action: "all"` is **rejected** — it used to wipe active keys too:
  `{"deleted":0,"error":"unsafe action rejected", ...}`
- Active keys are never touched by the bulk (`disabled`) flow
- Provider is reloaded after any deletion

---

## Prefix history

These endpoints previously lived at `/admin/api/keys/bulk-add`,
`/admin/api/keys/count`, and `/admin/api/keys/bulk-delete`. Those paths are
gone (404/405) — update scripts to the `/admin/api/injection/*` prefix.
