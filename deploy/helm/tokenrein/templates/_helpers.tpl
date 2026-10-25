{{- define "tokenrein.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{- define "tokenrein.fullname" -}}
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

{{- define "tokenrein.labels" -}}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{ include "tokenrein.selectorLabels" . }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{- define "tokenrein.selectorLabels" -}}
app.kubernetes.io/name: {{ include "tokenrein.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/component: proxy
{{- end }}

{{- define "tokenrein.serviceAccountName" -}}
{{- if .Values.serviceAccount.create }}
{{- default (include "tokenrein.fullname" .) .Values.serviceAccount.name }}
{{- else }}
{{- default "default" .Values.serviceAccount.name }}
{{- end }}
{{- end }}

{{- define "tokenrein.secretName" -}}
{{- default (include "tokenrein.fullname" .) .Values.secrets.existingSecret }}
{{- end }}

{{- define "tokenrein.redisHost" -}}
{{- printf "%s-redis" (include "tokenrein.fullname" .) }}
{{- end }}

{{- define "tokenrein.postgresHost" -}}
{{- printf "%s-postgresql" (include "tokenrein.fullname" .) }}
{{- end }}

{{/* Whether the proxy is given TOKENREIN_DATABASE_URL. */}}
{{- define "tokenrein.hasDatabase" -}}
{{- if or .Values.postgresql.enabled .Values.externalDatabase.enabled }}true{{ end }}
{{- end }}

{{- define "tokenrein.image" -}}
{{- printf "%s:%s" .Values.image.repository (default .Chart.AppVersion .Values.image.tag) }}
{{- end }}
