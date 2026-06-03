{{/*
Pods are labelled app.kubernetes.io/name: postgres (NOT the chart name) so the
parent patom chart's allow-egress-postgres NetworkPolicy, which targets that
label, matches the bundled DB.
*/}}
{{- define "postgresql.labels" -}}
app.kubernetes.io/name: postgres
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
app.kubernetes.io/component: database
{{- end -}}

{{- define "postgresql.selectorLabels" -}}
app.kubernetes.io/name: postgres
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}
