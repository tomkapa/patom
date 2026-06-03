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

echo "==> assert the bundled subchart carries no SaaS-internal coupling"
# Scope to the subchart's own templates (the app's secret sync-wave decoupling is
# tracked separately): the standalone DB must ship no backup CronJob, no ArgoCD
# sync-waves, and no hardcoded internal storageClass.
SUB="$(helm template patom "$CHART" -f "$EXAMPLE" \
  -s charts/postgresql/templates/statefulset.yaml \
  -s charts/postgresql/templates/service.yaml \
  -s charts/postgresql/templates/secret.yaml \
  -s charts/postgresql/templates/networkpolicy.yaml)"
grep -q 'argocd.argoproj.io/sync-wave' <<<"$SUB" && fail "subchart must not carry ArgoCD sync-wave annotations"
grep -qi 'kind: CronJob' <<<"$SUB" && fail "standalone DB must not ship a backup CronJob"
grep -qi 'externalsecret' <<<"$SUB" && fail "standalone DB must not require External Secrets Operator"
grep -q 'hcloud-volumes' <<<"$SUB" && fail "standalone DB must not hardcode the hcloud-volumes storageClass"
pass "no sync-waves / CronJob / ESO / hcloud storageClass"

echo "✅ helm-test passed"
