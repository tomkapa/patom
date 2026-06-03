# Self-hosting Patom — published artifacts & install

How to deploy Patom on your own Kubernetes cluster from published artifacts: the
versioned Helm chart and container image, how to pin them, and how to sideload
into an air-gapped / mirrored registry. Pairs with the plain-Secret config path in
[`deploy/helm/patom/values.example.yaml`](../../deploy/helm/patom/values.example.yaml)
(issue #84).

## TL;DR

```bash
# Versioned chart from the public OCI registry — no repo clone, no credentials.
helm install patom oci://ghcr.io/tomkapa/charts/patom --version 0.1.0 \
  -f my-values.yaml
```

A single chart installs the app **and** a bundled Postgres (pgvector) — the
`postgresql` subchart, ON by default. The chart and the `ghcr.io/tomkapa/patom`
image both pull anonymously from public GHCR and ship a stable, **pinned** image
tag. No `imagePullSecrets`, no GHCR login.

> Prefer pinning a chart **`--version`** (see the [chart package page](https://github.com/tomkapa/patom/pkgs/container/patom%2Fpatom)
> for available versions). The `deploy/helm/patom` path in this repo is the same
> chart if you'd rather install from a checkout.

> **Standalone vs. managed DB.** The bundled Postgres is a single replica with
> **no backups or HA** — fine for evaluation and small self-hosts. For production,
> run your own managed pgvector Postgres: set `postgresql.enabled: false` and point
> `DATABASE_URL` at it (see [§2b](#2b-bring-your-own-managed-postgres)).

---

## 1. Published artifacts

Everything you install is published to **public** GitHub Container Registry
packages — anyone can pull, no authentication:

| Artifact | Reference |
|----------|-----------|
| Helm chart (app + bundled DB) | `oci://ghcr.io/tomkapa/charts/patom` (versioned, `--version X.Y.Z`) |
| App + API + SPA image | `ghcr.io/tomkapa/patom` |
| Postgres + pgvector (bundled DB) | `pgvector/pgvector:pg17` (Docker Hub, public) |

(There is no backup image to pull: the standalone chart ships no backup job —
backups are your responsibility, see [§5](#5-backups). Our SaaS backup tooling
lives separately in our infra repo.)

The chart is versioned independently of the app image: `helm install --version`
selects the **chart**, and the chart pins a specific **app image** tag in its
`values.yaml` — so a given chart version is fully reproducible.

### Pinned tags

The chart pins a specific app tag in
[`deploy/helm/patom/values.yaml`](../../deploy/helm/patom/values.yaml) (`image.tag`).
That pinned tag is your deployable, reproducible version — it is **not** rewritten
by our CI. (Our SaaS continuous-deploy floats to the latest `main` build through a
separate ArgoCD parameter, so it never moves your chart default.)

- **Do not deploy `:latest` in production.** It floats with every `main` build.
- To see available tags: the
  [GHCR package page](https://github.com/tomkapa/patom/pkgs/container/patom), or
  `crane ls ghcr.io/tomkapa/patom` / `skopeo list-tags docker://ghcr.io/tomkapa/patom`.
- Pin by **digest** for the strongest guarantee:
  `--set image.repository=ghcr.io/tomkapa/patom@sha256 ...` — or set `image.tag`
  to a `sha256:...` digest your tooling resolved.

---

## 2. Direct install (cluster with internet egress)

1. Get the example values and fill in every `CHANGE_ME`:

   ```bash
   # Extract the example values bundled in the chart (or copy from the repo):
   helm show values oci://ghcr.io/tomkapa/charts/patom --version 0.1.0 > my-values.yaml
   $EDITOR my-values.yaml          # secrets, ingress.host, OAuth client IDs, ...
   ```

   The chart's `values.example.yaml` uses the **plain Kubernetes Secret** path
   (`secret.create: true`) — no External Secrets Operator / OpenBao required.

   Set `postgresql.auth.password` to the **same** password you put in
   `DATABASE_URL` (the bundled DB and the app's DSN must agree).

2. Install (one chart — app + bundled DB):

   ```bash
   helm install patom oci://ghcr.io/tomkapa/charts/patom --version 0.1.0 \
     -f my-values.yaml
   ```

`imagePullSecrets` defaults to `[]`, so the public image pulls with no registry
credentials. You only need a pull secret if you mirror into a private registry
(next section).

### 2b. Bring your own managed Postgres

For production, point Patom at a managed pgvector Postgres instead of the bundled
single-replica DB:

```yaml
postgresql:
  enabled: false                      # don't deploy the bundled StatefulSet
secret:
  data:
    DATABASE_URL: "postgres://USER:PW@your-db.example.com:5432/patom?sslmode=require"
```

- The instance **must have the `pgvector` extension available** (managed Postgres
  on AWS RDS, Google Cloud SQL, Azure, Neon, Supabase, etc. all offer it). Patom's
  first migration runs `CREATE EXTENSION vector` itself.
- With the bundled DB disabled, **you own backups, HA, and upgrades** for that
  database — your provider's tooling handles it, not this chart.

---

## 2a. Domain, DNS & TLS

Patom serves the app, API, and SPA from **one domain**. Set it once:

```yaml
ingress:
  host: patom.example.com    # the single value to set
```

`ingress.host` is the source of truth — `PATOM_OAUTH_REDIRECT_BASE` and
`PATOM_WEB_BASE_URL` are derived as `https://<host>`, so they can't drift. (If an
external LB exposes a public hostname that differs from `ingress.host`, override
it with `app.publicUrl`.)

### DNS

Point a record for your host at your ingress controller's external address:

- Get the controller's external IP / hostname (nginx example):
  `kubectl get svc -n ingress-nginx ingress-nginx-controller`
- Create an **A**/**AAAA** record (IP) or **CNAME** (hostname) for
  `patom.example.com` → that address.

### TLS — pick one `ingress.tls.mode`

| Mode | What it does | Prerequisite |
|------|--------------|--------------|
| `cert-manager` (default) | cert-manager issues + auto-renews into `tls.secretName` via `tls.clusterIssuer` | cert-manager installed + a `ClusterIssuer`. HTTP-01 works for a single host; DNS-01 only if you need a wildcard cert. |
| `existing-secret` | Ingress uses a TLS Secret you provide; no cert-manager | `kubectl create secret tls patom-tls --cert=fullchain.pem --key=key.pem` (any CA, incl. internal) |
| `none` | No `tls` block; TLS terminated upstream (cloud LB / edge). `ssl-redirect` is disabled to avoid a redirect loop | Your LB / edge presents the cert for the host |

```yaml
# cert-manager (default)
ingress:
  host: patom.example.com
  tls: { mode: cert-manager, clusterIssuer: letsencrypt-prod, secretName: patom-tls }

# bring your own cert
ingress:
  host: patom.example.com
  tls: { mode: existing-secret, secretName: patom-tls }

# TLS terminated at a cloud LB / edge
ingress:
  host: patom.example.com
  tls: { mode: none }
```

### Single-origin vs. cross-subdomain

The default is **single-origin**: cookies are host-only and there is no CORS layer
— everything is served from your one host, which is what most self-hosters want.
Cross-subdomain cookie sharing / credentialed CORS (e.g. a separate marketing site
reading login state) is cloud-specific and **off by default**. Enable it only when
you split the app across hostnames:

```yaml
app:
  cookieDomain: ".example.com"               # cookies shared across *.example.com
  corsAllowedOrigins: ["https://www.example.com"]
```

---

## 3. Air-gapped / mirrored registry (sideload)

For clusters with no path to `ghcr.io`, mirror the pinned images into a registry
your nodes can reach.

### 3a. Pull + save (on a machine with internet)

```bash
TAG=bd8840814367              # the pinned app tag from the chart (see §1)
for img in \
  ghcr.io/tomkapa/patom:$TAG \
  pgvector/pgvector:pg17 ; do
    docker pull "$img"
done
docker save \
  ghcr.io/tomkapa/patom:$TAG \
  pgvector/pgvector:pg17 \
  -o patom-images-$TAG.tar
```

(Using a managed Postgres? Drop `pgvector/pgvector:pg17` — you only need the app
image.)

A convenience wrapper is provided:
[`deploy/airgap-save.sh`](../../deploy/airgap-save.sh) — `./deploy/airgap-save.sh <tag>`.

Pull the **chart** too (so you don't need the repo on the air-gapped side):

```bash
helm pull oci://ghcr.io/tomkapa/charts/patom --version 0.1.0   # → patom-0.1.0.tgz
```

### 3b. Transfer + load + push into your mirror

```bash
docker load -i patom-images-$TAG.tar
MIRROR=registry.internal:5000        # your registry
docker tag  ghcr.io/tomkapa/patom:$TAG $MIRROR/patom:$TAG
docker push $MIRROR/patom:$TAG
docker tag  pgvector/pgvector:pg17 $MIRROR/pgvector:pg17
docker push $MIRROR/pgvector:pg17
```

### 3c. Point the chart at the mirror

Install from the local chart tarball (from §3a), pointing images at your mirror:

```bash
helm install patom ./patom-0.1.0.tgz -f my-values.yaml \
  --set image.repository=registry.internal:5000/patom \
  --set image.tag=$TAG \
  --set postgresql.image=registry.internal:5000/pgvector:pg17
```

If the mirror requires auth, create a `dockerconfigjson` Secret and reference it:

```bash
kubectl create secret docker-registry mirror-cred \
  --docker-server=registry.internal:5000 \
  --docker-username=... --docker-password=... -n patom

helm install patom ./patom-0.1.0.tgz -f my-values.yaml \
  --set image.repository=registry.internal:5000/patom \
  --set image.tag=$TAG \
  --set-json 'imagePullSecrets=[{"name":"mirror-cred"}]'
```

---

## 4. Upgrades & rollback

- **Upgrade:** pick a newer chart version, then
  `helm upgrade patom oci://ghcr.io/tomkapa/charts/patom --version <new> -f my-values.yaml`.
  Each chart version pins its own app image tag, so bumping `--version` upgrades
  both together. The app runs **all pending sqlx migrations on boot before binding
  the listener**
  — a cold start runs 100+ of them. The chart's `startupProbe` budget covers this
  (`probes.startup.failureThreshold × periodSeconds` = 60 × 5s = **300s** by
  default); a very slow disk or a large migration may need a higher
  `probes.startup.failureThreshold`, or liveness will kill the pod mid-migration.
- **Rollback:** redeploy the previous pinned tag (`--set image.tag=<old>`), or
  `helm rollback patom`. Database migrations are forward-only — roll the image
  back only to a tag whose schema your DB still satisfies.

---

## 5. Backups

**Running a managed Postgres ([§2b](#2b-bring-your-own-managed-postgres))? Use your
provider's backups** — that's the recommended production setup and this chart owns
none of it.

The **bundled** `postgresql` subchart ships **no backup job** by design (it's the
standalone/evaluation DB). Back it up yourself with `pg_dump` against the
in-cluster Service:

```bash
# Dump (custom format) from a one-off job / your workstation via port-forward:
kubectl -n patom exec -i sts/patom-postgres -- \
  pg_dump -U patom -Fc patom > patom-$(date -u +%Y%m%dT%H%M%SZ).dump

# Restore into a fresh DB:
kubectl -n patom exec -i sts/patom-postgres -- \
  pg_restore -U patom -d patom --clean --if-exists < patom-<timestamp>.dump
```

The dump contains tenant data and `PATOM_MASTER_KEK`-encrypted MCP credentials —
store it **encrypted and private**, and keep `PATOM_MASTER_KEK` itself somewhere
separate (without it the encrypted columns can't be decrypted after a restore).
Schedule the dump with your own CronJob and ship it to object storage you control.

---

## 6. Secret & key rotation

All app secrets live in the `patom-config` Secret (plain-Secret path) or your
OpenBao KV (ESO path). The Deployment carries `reloader.stakater.com/auto: "true"`,
so if the [stakater/reloader](https://github.com/stakater/Reloader) controller is
installed, updating the Secret triggers a rolling restart automatically. Without
it, restart manually after any change:

```bash
kubectl -n patom rollout restart deploy/patom
```

### Rotatable any time

- **Provider / OAuth / search / embedding / S3 / telemetry keys**
  (`DEEPSEEK_API_KEY`, `GOOGLE_CLIENT_*`, `PATOM_GITHUB_*`, `BRAVE_SEARCH_API_KEY`,
  `EMBEDDING_API_KEY`, `PATOM_S3_*`, `HONEYCOMB_*` / `LANGFUSE_*`): update the value
  and restart. No data migration — these are used live, not stored encrypted.
- **Database password**: rotate it in Postgres, then update `DATABASE_URL` to match
  (and, for the **bundled** DB, `postgresql.auth.password`), and restart.
- **`PATOM_JWT_SECRET`** (HS256 session-cookie signing, ≥32 bytes): rotating it
  **invalidates all existing sessions** — every user must log in again. Otherwise
  safe; rotate on suspected compromise.

### ⚠️ `PATOM_MASTER_KEK` — treat as permanent

The master KEK is the root of the envelope encryption protecting **stored MCP
credentials**. Every encrypted record is stamped with a `key_version`, but the
current build loads exactly one KEK (version 1) from `PATOM_MASTER_KEK` and has
**no re-wrap path**. Therefore:

- **Do not change `PATOM_MASTER_KEK` on a running instance.** Existing credentials
  were sealed under the old key; under a new key they fail to decrypt and the app
  rejects them — every stored MCP connection breaks (silently, until next use).
- **Back it up independently of the database.** A DB restore is useless without the
  exact KEK that encrypted its secret columns (see [§5](#5-backups)).
- **If the key is compromised** and must change, the only recovery today is: set the
  new KEK, then **re-enter every MCP credential** through the app so each is
  re-sealed under it. Plan downtime for those integrations.

In-place KEK rotation (load old + new, re-wrap rows, retire the old version) is
modelled by `key_version` but **not yet implemented** — tracked as future work.

---

## 7. Observability & data egress

A fresh self-hosted install is **private by default** — nothing about your prompts
leaves the cluster:

- **No prompt content is captured.** The chart sets `PATOM_GENAI_CAPTURE_CONTENT=0`,
  so request/response text is never recorded onto spans or stderr.
- **No telemetry is exported off-cluster.** With no backend keys set, the app runs
  **console-only** (structured logs to stderr) — no OTLP exporter is built. The
  `HONEYCOMB_*` / `LANGFUSE_*` keys in `values.example.yaml` ship commented out.

To enable observability while keeping data on-box, run a collector **in your own
namespace** (an OpenTelemetry Collector, or self-hosted Langfuse) and point the app
at it — never at a vendor cloud. Uncomment the `env:` and matching `secret.data`
blocks in
[`values.example.yaml`](../../deploy/helm/patom/values.example.yaml):

```yaml
env:
  PATOM_GENAI_CAPTURE_CONTENT: "1"                       # opt in to content capture
  HONEYCOMB_BASE_URL: "http://otel-collector.patom.svc:4318"   # in-cluster OTLP/HTTP
secret:
  data:
    HONEYCOMB_API_KEY: "any-value"      # a local collector ignores the header value
```

The app appends `/v1/traces` to the base URL automatically. The Honeycomb path needs
only `HONEYCOMB_API_KEY` (value ignored by a local collector); the Langfuse path needs
both `LANGFUSE_PUBLIC_KEY` and `LANGFUSE_SECRET_KEY` with `LANGFUSE_BASE_URL` pointed
at your self-hosted Langfuse. Setting a cloud base URL (the upstream default) is what
sends data off-cluster — so only do that deliberately.

---

## Notes

- The image is source-available under FSL-1.1-Apache-2.0 (see the repo `LICENSE.md`).
  A public image adds no rights beyond the license. For a commercial license that
  lifts the Competing Use restriction, contact the maintainers.
- Maintainer / SaaS prerequisite: the GHCR packages above — including the chart
  package `charts/patom` — must be set to **public** visibility in GitHub package
  settings for anonymous pulls to work. (The chart package appears after the first
  `chart-release` run; set it public once.)
- **Releasing the chart:** bump `version` in `deploy/helm/patom/Chart.yaml`, commit,
  then push a tag `chart-vX.Y.Z` matching it. The `chart-release` workflow lints,
  packages, and pushes `oci://ghcr.io/tomkapa/charts/patom:X.Y.Z`.
