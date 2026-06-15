#!/usr/bin/env bash
# Chart-render gate for the patom Helm chart (CLAUDE.md §3). Asserts the bundled
# standalone Postgres subchart renders when enabled (default, self-host mode) and
# disappears when disabled (bring-your-own managed pgvector / SaaS). Runs in CI
# (.github/workflows/ci.yml `chart` job) and locally: ./ci/helm-test.sh
set -euo pipefail

CHART="deploy/helm/patom"
EXAMPLE="${CHART}/values.example.yaml"

fail() { echo "::error::helm-test: $1"; exit 1; }
pass() { echo "  ✓ $1"; }

echo "==> helm lint"
helm lint "$CHART" -f "$EXAMPLE" >/dev/null || fail "helm lint failed"
pass "lint clean"

echo "==> render A: bundled DB (default postgresql.enabled=true)"
A="$(helm template patom "$CHART" -f "$EXAMPLE")"
grep -qE '^kind: StatefulSet$' <<<"$A" || fail "bundled StatefulSet missing when postgresql.enabled=true"
grep -q 'app.kubernetes.io/name: postgres' <<<"$A" || fail "bundled DB must label pods app.kubernetes.io/name=postgres (parent allow-egress-postgres targets it)"
grep -q 'allow-ingress-to-postgres' <<<"$A" || fail "bundled DB needs its own ingress NetworkPolicy under the parent default-deny"
grep -qE '^kind: Deployment$' <<<"$A" || fail "app Deployment missing"
pass "StatefulSet + ingress policy + Deployment present"

echo "==> render B: external DB (postgresql.enabled=false)"
B="$(helm template patom "$CHART" -f "$EXAMPLE" --set postgresql.enabled=false)"
grep -qE '^kind: StatefulSet$' <<<"$B" && fail "StatefulSet must NOT render when postgresql.enabled=false (managed/external DB)"
grep -qE '^kind: Deployment$' <<<"$B" || fail "app Deployment missing in external-DB mode"
pass "no StatefulSet; app Deployment present"

echo "==> assert no SaaS-internal coupling in the default customer render"
# The whole default render must carry no ArgoCD sync-waves, no ResourceQuota, no
# backup CronJob, no hcloud storageClass — those are SaaS-only and gated off.
grep -q 'argocd.argoproj.io/sync-wave' <<<"$A" && fail "no ArgoCD sync-wave should render by default (argocd.syncWaves off)"
grep -qE '^kind: ResourceQuota$' <<<"$A" && fail "no ResourceQuota should render by default (resourceQuota.enabled off)"
grep -qi 'kind: CronJob' <<<"$A" && fail "standalone DB must not ship a backup CronJob"
grep -q 'hcloud-volumes' <<<"$A" && fail "must not hardcode the hcloud-volumes storageClass"
pass "no sync-waves / ResourceQuota / CronJob / hcloud storageClass"

echo "==> assert the SaaS flags re-enable sync-waves + quota"
S="$(helm template patom "$CHART" -f "$EXAMPLE" --set argocd.syncWaves=true --set resourceQuota.enabled=true)"
grep -q 'argocd.argoproj.io/sync-wave: "-1"' <<<"$S" || fail "argocd.syncWaves=true must add the sync-wave annotation"
grep -qE '^kind: ResourceQuota$' <<<"$S" || fail "resourceQuota.enabled=true must render the ResourceQuota"
pass "argocd.syncWaves + resourceQuota.enabled honoured"

echo "==> package the chart (the artifact chart-release publishes to OCI)"
pkgdir="$(mktemp -d)"
helm package "$CHART" --destination "$pkgdir" >/dev/null || fail "helm package failed"
ver="$(helm show chart "$CHART" | awk '/^version:/ {print $2}')"
test -f "$pkgdir/patom-${ver}.tgz" || fail "expected packaged chart patom-${ver}.tgz"
# Capture the listing first, then grep it: piping `tar … | grep -q` makes grep
# close the pipe on its first match, so tar dies with SIGPIPE and `pipefail`
# turns the whole pipeline non-zero even though the subchart was found.
listing="$(tar tzf "$pkgdir/patom-${ver}.tgz")"
grep -q 'patom/charts/postgresql/Chart.yaml' <<<"$listing" \
  || fail "packaged chart is missing the vendored postgresql subchart"
rm -rf "$pkgdir"
pass "packages to patom-${ver}.tgz with the postgresql subchart"

echo "✅ helm-test passed"
