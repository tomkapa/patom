# Self-hosting Patom — image distribution & install

How to deploy Patom on your own Kubernetes cluster: where the container image
comes from, how to pin it, and how to sideload it into an air-gapped / mirrored
registry. Pairs with the plain-Secret config path in
[`deploy/helm/patom/values.example.yaml`](../../deploy/helm/patom/values.example.yaml)
(issue #84).

## TL;DR

```bash
# Public image, no registry credentials needed.
helm install patom deploy/helm/patom -f deploy/helm/patom/values.example.yaml
helm install patom-postgres deploy/helm/postgres   # sibling DB chart
```

The chart defaults pull `ghcr.io/tomkapa/patom` anonymously and ship a stable,
**pinned** image tag. No `imagePullSecrets`, no GHCR login.

---

## 1. Image distribution

Patom's images are published to **public** GitHub Container Registry packages —
anyone can pull them, no authentication:

| Component | Image |
|-----------|-------|
| App + API + SPA | `ghcr.io/tomkapa/patom` |
| Postgres backup job | `ghcr.io/tomkapa/patom-backup` |
| Postgres + pgvector | `pgvector/pgvector:pg17` (Docker Hub, public) |

### Pinned tags

The charts pin a specific tag in their `values.yaml`
(`image.tag` in [`deploy/helm/patom/values.yaml`](../../deploy/helm/patom/values.yaml)
and `backup.imageTag` in
[`deploy/helm/postgres/values.yaml`](../../deploy/helm/postgres/values.yaml)).
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

1. Copy the example values and fill in every `CHANGE_ME`:

   ```bash
   cp deploy/helm/patom/values.example.yaml my-values.yaml
   $EDITOR my-values.yaml          # secrets, ingress.host, OAuth client IDs, ...
   ```

   `values.example.yaml` uses the **plain Kubernetes Secret** path
   (`secret.create: true`) — no External Secrets Operator / OpenBao required.

2. Install:

   ```bash
   helm install patom-postgres deploy/helm/postgres
   helm install patom          deploy/helm/patom -f my-values.yaml
   ```

`imagePullSecrets` defaults to `[]`, so the public image pulls with no registry
credentials. You only need a pull secret if you mirror into a private registry
(next section).

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
TAG=bd8840814367              # the pinned tag from the charts (see §1)
for img in \
  ghcr.io/tomkapa/patom:$TAG \
  ghcr.io/tomkapa/patom-backup:$TAG \
  pgvector/pgvector:pg17 ; do
    docker pull "$img"
done
docker save \
  ghcr.io/tomkapa/patom:$TAG \
  ghcr.io/tomkapa/patom-backup:$TAG \
  pgvector/pgvector:pg17 \
  -o patom-images-$TAG.tar
```

A convenience wrapper is provided:
[`deploy/airgap-save.sh`](../../deploy/airgap-save.sh) — `./deploy/airgap-save.sh <tag>`.

### 3b. Transfer + load + push into your mirror

```bash
docker load -i patom-images-$TAG.tar
MIRROR=registry.internal:5000        # your registry
for img in patom patom-backup ; do
    docker tag ghcr.io/tomkapa/$img:$TAG $MIRROR/$img:$TAG
    docker push $MIRROR/$img:$TAG
done
docker tag  pgvector/pgvector:pg17 $MIRROR/pgvector:pg17
docker push $MIRROR/pgvector:pg17
```

### 3c. Point the charts at the mirror

```bash
helm install patom-postgres deploy/helm/postgres \
  --set image=registry.internal:5000/pgvector \
  --set backup.image=registry.internal:5000/patom-backup \
  --set backup.imageTag=$TAG

helm install patom deploy/helm/patom -f my-values.yaml \
  --set image.repository=registry.internal:5000/patom \
  --set image.tag=$TAG
```

If the mirror requires auth, create a `dockerconfigjson` Secret and reference it:

```bash
kubectl create secret docker-registry mirror-cred \
  --docker-server=registry.internal:5000 \
  --docker-username=... --docker-password=... -n patom

helm install patom deploy/helm/patom -f my-values.yaml \
  --set image.repository=registry.internal:5000/patom \
  --set image.tag=$TAG \
  --set-json 'imagePullSecrets=[{"name":"mirror-cred"}]'
```

---

## 4. Upgrades & rollback

- **Upgrade:** pick a newer pinned tag, then
  `helm upgrade patom deploy/helm/patom -f my-values.yaml --set image.tag=<new>`.
  The app runs all pending sqlx migrations on boot before binding the listener;
  the chart's `startupProbe` budget covers that.
- **Rollback:** redeploy the previous pinned tag (`--set image.tag=<old>`), or
  `helm rollback patom`. Database migrations are forward-only — roll the image
  back only to a tag whose schema your DB still satisfies.

---

## 5. Observability & data egress

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
- Maintainer / SaaS prerequisite: the GHCR packages above must be set to **public**
  visibility in GitHub package settings for anonymous pulls to work.
