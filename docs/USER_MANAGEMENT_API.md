# User Management API

DB-backed user management for multi-tenant IronClaw deployments. Covers admin user CRUD, self-service profile, API token management, and usage reporting.

## Authentication

All endpoints require `Authorization: Bearer <token>`. Tokens are either:
- **Env-var tokens** — configured via `GATEWAY_AUTH_TOKEN` (single-user) at startup
- **DB-backed tokens** — created via `POST /api/tokens` or `POST /api/admin/users`

DB tokens are SHA-256 hashed at rest; plaintext is returned exactly once at creation time.

Auth is cached in a bounded LRU (1024 entries, 60s TTL). Suspending a user or revoking a token may take up to 60s to take effect.

## Roles

| Role | Scope |
|------|-------|
| `admin` | Full access to all endpoints |
| `member` | Self-service profile + own token management only |

Endpoints marked **Admin** return `403 Forbidden` for `member` role.

---

## Admin: Users

### POST /api/admin/users

Create a new user. Returns the user record and a one-time plaintext API token.

**Auth:** Admin

**Request body:**

```json
{
  "display_name": "Alice Smith",
  "email": "alice@example.com",
  "role": "member"
}
```

| Field | Type | Required | Default | Notes |
|-------|------|----------|---------|-------|
| `display_name` | string | yes | | |
| `email` | string | no | `null` | Must be unique if provided |
| `role` | string | no | `"member"` | `"admin"` or `"member"` |

**Response:** `200 OK`

```json
{
  "id": "550e8400-e29b-41d4-a716-446655440000",
  "email": "alice@example.com",
  "display_name": "Alice Smith",
  "status": "active",
  "role": "member",
  "token": "a1b2c3d4e5f6...64-char hex...",
  "created_at": "2026-03-25T12:00:00+00:00",
  "created_by": "admin-user-id"
}
```

The `token` field is the plaintext API token. It is shown **only once** — store it securely.

**Errors:** `400` (missing display_name, invalid role), `403` (not admin), `503` (no database)

---

### GET /api/admin/users

List all users.

**Auth:** Admin

**Response:** `200 OK`

```json
{
  "users": [
    {
      "id": "550e8400-...",
      "email": "alice@example.com",
      "display_name": "Alice Smith",
      "status": "active",
      "role": "member",
      "created_at": "2026-03-25T12:00:00+00:00",
      "updated_at": "2026-03-25T12:00:00+00:00",
      "last_login_at": "2026-03-25T14:30:00+00:00",
      "created_by": "admin-user-id"
    }
  ]
}
```

---

### GET /api/admin/users/{id}

Get a single user by ID.

**Auth:** Admin

**Response:** `200 OK`

```json
{
  "id": "550e8400-...",
  "email": "alice@example.com",
  "display_name": "Alice Smith",
  "status": "active",
  "role": "member",
  "created_at": "2026-03-25T12:00:00+00:00",
  "updated_at": "2026-03-25T12:00:00+00:00",
  "last_login_at": "2026-03-25T14:30:00+00:00",
  "created_by": "admin-user-id",
  "metadata": {}
}
```

**Errors:** `404` (user not found), `403` (not admin)

---

### PATCH /api/admin/users/{id}

Update a user's display name and/or metadata. Omitted fields are left unchanged.

**Auth:** Admin

**Request body:**

```json
{
  "display_name": "Alice Johnson",
  "metadata": {"department": "engineering"}
}
```

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `display_name` | string | no | |
| `metadata` | object | no | Replaces entire metadata object (merge patch) |

**Response:** `200 OK` — returns the full updated user record (same shape as GET detail, without `last_login_at`/`created_by`).

**Errors:** `404` (user not found), `403` (not admin)

---

### POST /api/admin/users/{id}/suspend

Suspend a user. Suspended users cannot authenticate (DB auth checks user status).

**Auth:** Admin

**Response:** `200 OK`

```json
{
  "id": "550e8400-...",
  "status": "suspended"
}
```

**Errors:** `404` (user not found), `403` (not admin)

---

### POST /api/admin/users/{id}/activate

Re-activate a suspended user.

**Auth:** Admin

**Response:** `200 OK`

```json
{
  "id": "550e8400-...",
  "status": "active"
}
```

**Errors:** `404` (user not found), `403` (not admin)

---

### DELETE /api/admin/users/{id}

Permanently delete a user and all associated data (tokens, jobs, conversations, memory, routines, settings, secrets).

**Auth:** Admin

**Response:** `200 OK`

```json
{
  "id": "550e8400-...",
  "deleted": true
}
```

**Errors:** `404` (user not found), `403` (not admin)

**Cascade:** Deletes from `api_tokens`, `agent_jobs`, `conversations`, `memory_documents`, `routines`, `secrets`, `settings`, `wasm_tools`, and related tables. On PostgreSQL this uses FK cascades; on libSQL it uses explicit deletes.

---

## Admin: Usage

### GET /api/admin/usage

Per-user LLM usage statistics aggregated from `llm_calls` via `agent_jobs.user_id`.

**Auth:** Admin

**Query parameters:**

| Param | Type | Default | Notes |
|-------|------|---------|-------|
| `user_id` | string | all users | Filter to a single user |
| `period` | string | `"day"` | `"day"` (24h), `"week"` (7d), or `"month"` (30d) |

**Response:** `200 OK`

```json
{
  "period": "week",
  "since": "2026-03-18T12:00:00+00:00",
  "usage": [
    {
      "user_id": "alice-id",
      "model": "claude-sonnet-4-5-20250514",
      "call_count": 42,
      "input_tokens": 150000,
      "output_tokens": 30000,
      "total_cost": "1.23"
    }
  ]
}
```

---

## Self-Service: Profile

### GET /api/profile

Get the authenticated user's own profile.

**Auth:** Any authenticated user

**Response:** `200 OK`

```json
{
  "id": "550e8400-...",
  "email": "alice@example.com",
  "display_name": "Alice Smith",
  "status": "active",
  "role": "member",
  "created_at": "2026-03-25T12:00:00+00:00",
  "last_login_at": "2026-03-25T14:30:00+00:00"
}
```

---

### PATCH /api/profile

Update the authenticated user's own display name and/or metadata.

**Auth:** Any authenticated user

**Request body:**

```json
{
  "display_name": "Alice Johnson",
  "metadata": {"theme": "dark"}
}
```

**Response:** `200 OK`

```json
{
  "id": "550e8400-...",
  "display_name": "Alice Johnson",
  "updated": true
}
```

---

## Self-Service: Tokens

### POST /api/tokens

Create a new API token for the authenticated user. Admins can optionally create tokens for other users by including `user_id`.

**Auth:** Any authenticated user

**Request body:**

```json
{
  "name": "CI pipeline",
  "expires_in_days": 90,
  "user_id": "other-user-id"
}
```

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `name` | string | yes | Human-readable label |
| `expires_in_days` | integer | no | `null` = never expires |
| `user_id` | string | no | Admin-only; create token for another user |

**Response:** `200 OK`

```json
{
  "token": "a1b2c3d4...64-char hex...",
  "id": "token-uuid",
  "name": "CI pipeline",
  "token_prefix": "a1b2c3d4",
  "expires_at": "2026-06-23T12:00:00+00:00",
  "created_at": "2026-03-25T12:00:00+00:00"
}
```

The `token` field is shown **only once**.

---

### GET /api/tokens

List the authenticated user's tokens. Token hashes are never returned.

**Auth:** Any authenticated user

**Response:** `200 OK`

```json
{
  "tokens": [
    {
      "id": "token-uuid",
      "name": "CI pipeline",
      "token_prefix": "a1b2c3d4",
      "expires_at": "2026-06-23T12:00:00+00:00",
      "last_used_at": "2026-03-25T14:00:00+00:00",
      "created_at": "2026-03-25T12:00:00+00:00",
      "revoked_at": null
    }
  ]
}
```

---

### DELETE /api/tokens/{id}

Revoke one of the authenticated user's tokens. Users can only revoke their own tokens.

**Auth:** Any authenticated user

**Path:** `id` — UUID of the token to revoke

**Response:** `200 OK`

```json
{
  "status": "revoked",
  "id": "token-uuid"
}
```

**Errors:** `400` (invalid UUID), `404` (token not found or belongs to another user)

---

## Error Format

All error responses return a plain text body with the error message and the corresponding HTTP status code:

| Code | Meaning |
|------|---------|
| `400` | Bad request (missing fields, invalid input) |
| `401` | Missing or invalid bearer token |
| `403` | Authenticated but insufficient role (member accessing admin endpoint) |
| `404` | Resource not found |
| `503` | Database not available |
| `500` | Internal server error |

---

## Database Schema

### users

| Column | Type (PG / libSQL) | Notes |
|--------|--------------------|-------|
| `id` | `UUID` / `TEXT` | Primary key, UUID v4 |
| `email` | `TEXT UNIQUE` | Nullable |
| `display_name` | `TEXT NOT NULL` | |
| `status` | `TEXT NOT NULL` | `"active"` or `"suspended"` |
| `role` | `TEXT NOT NULL` | `"admin"` or `"member"` |
| `created_at` | `TIMESTAMPTZ` / `TEXT` | |
| `updated_at` | `TIMESTAMPTZ` / `TEXT` | |
| `last_login_at` | `TIMESTAMPTZ` / `TEXT` | Nullable |
| `created_by` | `TEXT` | Nullable, references `users.id` |
| `metadata` | `JSONB` / `TEXT` | Default `{}` |

### api_tokens

| Column | Type (PG / libSQL) | Notes |
|--------|--------------------|-------|
| `id` | `UUID` / `TEXT` | Primary key |
| `user_id` | `TEXT NOT NULL` | FK to `users.id` (PG cascades; libSQL uses explicit cleanup) |
| `token_hash` | `BYTEA` / `BLOB` | SHA-256 of hex-encoded plaintext |
| `token_prefix` | `TEXT NOT NULL` | First 8 chars for identification |
| `name` | `TEXT NOT NULL` | Human-readable label |
| `expires_at` | `TIMESTAMPTZ` / `TEXT` | Nullable |
| `last_used_at` | `TIMESTAMPTZ` / `TEXT` | Nullable |
| `created_at` | `TIMESTAMPTZ` / `TEXT` | |
| `revoked_at` | `TIMESTAMPTZ` / `TEXT` | Nullable; set on revocation |
