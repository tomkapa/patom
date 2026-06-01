{{- define "patom.name" -}}patom{{- end -}}

{{- define "patom.labels" -}}
app.kubernetes.io/name: patom
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end -}}

{{- define "patom.selectorLabels" -}}
app.kubernetes.io/name: patom
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{/*
Browser-facing origin, keyed off the single customer domain (ingress.host).
Drives PATOM_OAUTH_REDIRECT_BASE / PATOM_WEB_BASE_URL so they cannot drift from
the ingress host. app.publicUrl overrides only when an external LB exposes a
hostname that differs from ingress.host; otherwise derive https://<host> (TLS is
always https to the browser, even when terminated upstream / edge mode).
*/}}
{{- define "patom.publicUrl" -}}
{{- if .Values.app.publicUrl -}}
{{- .Values.app.publicUrl -}}
{{- else -}}
https://{{ required "ingress.host is required to derive the public URL" .Values.ingress.host }}
{{- end -}}
{{- end -}}
