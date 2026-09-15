{{- define "cart.name" -}}{{ default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}{{- end -}}
{{- define "cart.fullname" -}}
{{- if .Values.fullnameOverride -}}{{ .Values.fullnameOverride | trunc 63 | trimSuffix "-" }}
{{- else -}}{{- $name := default .Chart.Name .Values.nameOverride -}}
{{- if contains $name .Release.Name -}}{{ .Release.Name | trunc 63 | trimSuffix "-" }}
{{- else -}}{{ printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" }}{{- end -}}{{- end -}}
{{- end -}}
{{- define "cart.labels" -}}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{ include "cart.selectorLabels" . }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end -}}
{{- define "cart.selectorLabels" -}}
app.kubernetes.io/name: {{ include "cart.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}
{{/* cart-config 名:默认 <fullname>-config(按 release 唯一,多 cart 同 ns 不撞);可用 configMapName 覆盖。
     ⚠️ ModelRoute.cart.outputConfigMap 必须指到这个名。 */}}
{{- define "cart.configMapName" -}}
{{- .Values.configMapName | default (printf "%s-config" (include "cart.fullname" .)) -}}
{{- end -}}
