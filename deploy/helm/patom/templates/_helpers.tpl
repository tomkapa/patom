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
