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
     ModelRoute.cart.outputConfigMap 可指这个名,也可指某条覆盖层 CM。 */}}
{{- define "cart.configMapName" -}}
{{- .Values.configMapName | default (printf "%s-config" (include "cart.fullname" .)) -}}
{{- end -}}

{{/* ---- configOverlays(覆盖层):同一个 CM 里的额外 key,按序追加 -c,后者覆盖前者 ---- */}}

{{/* 校验:key 合法、不重名、autoconfig 最多一条且不带 content。 */}}
{{- define "cart.configOverlays.check" -}}
{{- $seen := dict -}}
{{- $auto := 0 -}}
{{- range $i, $o := .Values.configOverlays -}}
{{- $k := $o.key | default "" -}}
{{- if not $k -}}{{- fail (printf "configOverlays[%d]:必须设 key" $i) -}}{{- end -}}
{{- if not (regexMatch "^[-._a-zA-Z0-9]+$" $k) -}}
{{- fail (printf "configOverlays[%d].key=%q 非法:ConfigMap key 只能用字母/数字/`-`/`_`/`.`" $i $k) -}}
{{- end -}}
{{- if eq $k "config.yaml" -}}
{{- fail (printf "configOverlays[%d].key 不能是 config.yaml(那是 baseConfig 的 key)" $i) -}}
{{- end -}}
{{- if hasKey $seen $k -}}{{- fail (printf "configOverlays:key %q 重复" $k) -}}{{- end -}}
{{- $_ := set $seen $k true -}}
{{- if $o.autoconfig -}}
{{- $auto = add1 $auto -}}
{{- if hasKey $o "content" -}}
{{- fail (printf "configOverlays[%d](%s):autoconfig 的 key 由 autoconfig 写,chart 不渲染它 —— 模板一出这个 key,helm upgrade 就会盖掉写进去的 workers" $i $k) -}}
{{- end -}}
{{- else if not (hasKey $o "content") -}}
{{- fail (printf "configOverlays[%d](%s):要么给 content,要么标 autoconfig: true" $i $k) -}}
{{- end -}}
{{- end -}}
{{- if gt $auto 1 -}}
{{- fail (printf "configOverlays:最多一条 autoconfig: true,现在有 %d 条" $auto) -}}
{{- end -}}
{{- end -}}

{{/* 标了 autoconfig: true 那条的 key(没有则空)。ModelRoute.cart.outputKey 用它。 */}}
{{- define "cart.autoconfigKey" -}}
{{- range .Values.configOverlays -}}{{- if .autoconfig -}}{{- .key -}}{{- end -}}{{- end -}}
{{- end -}}

{{/* 全部配置文件路径,空格分隔,按加载顺序(底稿在前)。 */}}
{{- define "cart.configFiles" -}}
{{- $files := list "/workspace/configs/config.yaml" -}}
{{- range .Values.configOverlays -}}
{{- $files = append $files (printf "/workspace/configs/%s" .key) -}}
{{- end -}}
{{- join " " $files -}}
{{- end -}}
