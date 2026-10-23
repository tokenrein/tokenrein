{{- define "tokenlens.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{- define "tokenlens.fullname" -}}
{{- if .Values.fullnameOverride }}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- $name := default .Chart.Name .Values.nameOverride }}
{{- if contains $name .Release.Name }}
{{- .Release.Name | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" }}
{{- end }}
{{- end }}
{{- end }}

{{- define "tokenlens.labels" -}}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{ include "tokenlens.selectorLabels" . }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{- define "tokenlens.selectorLabels" -}}
app.kubernetes.io/name: {{ include "tokenlens.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/component: proxy
{{- end }}

{{- define "tokenlens.serviceAccountName" -}}
{{- if .Values.serviceAccount.create }}
{{- default (include "tokenlens.fullname" .) .Values.serviceAccount.name }}
{{- else }}
{{- default "default" .Values.serviceAccount.name }}
{{- end }}
{{- end }}

{{- define "tokenlens.secretName" -}}
{{- default (include "tokenlens.fullname" .) .Values.secrets.existingSecret }}
{{- end }}

{{- define "tokenlens.redisHost" -}}
{{- printf "%s-redis" (include "tokenlens.fullname" .) }}
{{- end }}

{{- define "tokenlens.postgresHost" -}}
{{- printf "%s-postgresql" (include "tokenlens.fullname" .) }}
{{- end }}

{{/* Whether the proxy is given TL_DATABASE_URL. */}}
{{- define "tokenlens.hasDatabase" -}}
{{- if or .Values.postgresql.enabled .Values.externalDatabase.enabled }}true{{ end }}
{{- end }}

{{- define "tokenlens.image" -}}
{{- printf "%s:%s" .Values.image.repository (default .Chart.AppVersion .Values.image.tag) }}
{{- end }}
