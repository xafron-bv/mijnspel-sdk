---
title: Local development
sidebar_position: 8
---

# Local development

You develop and test your WASM against the published Docker image
`ghcr.io/xafron-bv/mijnspel-dev-server:latest`. The image bundles
the full host (an AWS-API emulator plus the host binaries) and
boots offline with no AWS credentials.

## One-shot run

```
docker run --rm -p 3000:3000 \
    -v $PWD/local-wasm:/app/local-wasm \
    ghcr.io/xafron-bv/mijnspel-dev-server:latest
```

Inside the container:

1. MiniStack starts on port 4566 (only exposed inside the container
   unless you `-p 4566:4566`).
2. Bootstrap creates DDB tables / S3 bucket / SQS queues / Cognito
   user pool that match production.
3. The mounted `/app/local-wasm/` is scanned. Each
   `<appId>/{runtime,migration}.wasm` is sha256'd, uploaded to the
   in-container S3 under the production content-addressed key, and
   registered in `app-registry.<appId>`.
4. The host binds on port 3000 inside the container (mapped to your
   machine's `:3000` by `-p 3000:3000`).

Your game frontend points at `http://localhost:3000` and uses
`?appId=<your-app-id>` (matching the directory name you mounted).

## Drop-in WASM workflow

```
local-wasm/
└── my-game/
    ├── runtime.wasm       (your built artifact)
    └── migration.wasm     (your built artifact)
```

After rebuilding either artifact, send the container `SIGHUP` to
re-scan and pick up new bytes — no restart needed:

```
docker kill --signal SIGHUP <container-id>
```

Or just stop and re-run the container.

## Authentication

The bootstrap seeds a `_test_admin` Cognito user and a generic test
user. Sign your frontend in via:

```
POST /auth/login?appId=<your-app-id>
{ "username": "_test_admin", "password": "..." }
```

(See the local backend's bootstrap output for the exact seeded
credentials, or seed your own with the standard `/auth/signup`
endpoint — MiniStack auto-confirms.)

WS upgrades work the same way as production: pass the ID token as a
`Bearer` header on the upgrade.

## Configuration knobs

The image accepts two env vars game devs typically care about:

- `LOCAL_BACKEND_PORT` (default `3000`) — port the host binds inside
  the container. Override when running multiple instances:

  ```
  -e LOCAL_BACKEND_PORT=3010 -p 3010:3010
  ```

- `AUTH_ALLOWED_ORIGINS` (default `localhost:*`, `127.0.0.1:*`) —
  comma-separated CORS origin allowlist. Override when your frontend
  is on a Cloudflare Pages preview URL or a different host:

  ```
  -e AUTH_ALLOWED_ORIGINS=https://*.pages.dev,http://localhost:*
  ```

## What's *different* from production

This is a developer environment — close to production, but not
identical:

- No real S3 / DDB / SQS / Cognito; everything is MiniStack.
- The `app-registry` lives inside the container and disappears when
  the container stops. Production registration is the manual intake
  protocol.
- The local Cognito emulator's JWKS URL and `iss` claim differ from
  real AWS Cognito; the host honors `COGNITO_JWKS_URL_OVERRIDE` and
  `COGNITO_ISSUER_OVERRIDE` env vars to bridge that gap. The image
  sets them automatically.
- The emulator does not enforce API Gateway authorizers — irrelevant
  here because the in-container API Gateway simulator enforces them
  itself.

These differences only matter if your game directly probes Cognito's
JWKS endpoint or inspects JWT `iss` claims. If you're using a
standard OAuth/OIDC client, you won't notice them.

## Talking to MiniStack directly (rare)

Bind `4566` to the host:

```
docker run --rm -p 3000:3000 -p 4566:4566 \
    -v $PWD/local-wasm:/app/local-wasm \
    ghcr.io/xafron-bv/mijnspel-dev-server:latest
```

Then point `awslocal` (or `aws --endpoint-url=http://localhost:4566`)
at it. Useful for inspecting raw DDB rows during debugging.
