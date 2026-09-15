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

{{/* ---- extraConfigs(覆盖层):底稿之外再挂 N 个 CM,按序追加 -c,后者覆盖前者 ---- */}}

{{/* 解析+校验单条,返回 YAML(调用方 fromYaml)。入参 (list $root $entry $index)。 */}}
{{- define "cart.extraConfig.resolve" -}}
{{- $root := index . 0 -}}
{{- $e := index . 1 -}}
{{- $i := index . 2 -}}
{{- $name := $e.name | default "" -}}
{{- if not $name -}}
{{- fail (printf "extraConfigs[%d]:必须设 name" $i) -}}
{{- end -}}
{{- if not (regexMatch "^[a-z0-9]([-a-z0-9]*[a-z0-9])?$" $name) -}}
{{- fail (printf "extraConfigs[%d].name=%q 非法:只能小写字母/数字/中划线(RFC 1123)" $i $name) -}}
{{- end -}}
{{- if gt (len $name) 40 -}}
{{- fail (printf "extraConfigs[%d].name=%q 过长(%d>40):生成的 CM 名会超 63 字符" $i $name (len $name)) -}}
{{- end -}}
{{- $inline := hasKey $e "content" -}}
{{- $ref := and (hasKey $e "existingConfigMap") $e.existingConfigMap -}}
{{- if and $inline $ref -}}
{{- fail (printf "extraConfigs[%d](%s):content 与 existingConfigMap 只能二选一" $i $name) -}}
{{- end -}}
{{- if not (or $inline $ref) -}}
{{- fail (printf "extraConfigs[%d](%s):必须给 content 或 existingConfigMap 之一" $i $name) -}}
{{- end -}}
{{- $key := $e.key | default "config.yaml" -}}
{{- $auto := $e.autoconfig | default false -}}
{{- $cm := ternary (printf "%s-cfg-%s" (include "cart.fullname" $root) $name) ($e.existingConfigMap | toString) $inline -}}
configMap: {{ $cm | quote }}
key: {{ $key | quote }}
inline: {{ $inline }}
autoconfig: {{ $auto }}
optional: {{ $e.optional | default false }}
volume: {{ printf "cfg-%s" $name | quote }}
mountPath: {{ printf "/workspace/configs.d/%s" $name | quote }}
file: {{ printf "/workspace/configs.d/%s/%s" $name $key | quote }}
{{- end -}}

{{/* 全量校验:逐条 resolve,再查 name 不重复、autoconfig 不超过一条。 */}}
{{- define "cart.extraConfigs.check" -}}
{{- $seen := dict -}}
{{- range $i, $e := .Values.extraConfigs -}}
{{- $_ := include "cart.extraConfig.resolve" (list $ $e $i) -}}
{{- if hasKey $seen $e.name -}}
{{- fail (printf "extraConfigs:name %q 重复" $e.name) -}}
{{- end -}}
{{- $_ := set $seen $e.name true -}}
{{- end -}}
{{- $auto := 0 -}}
{{- range .Values.extraConfigs -}}
{{- if .autoconfig -}}{{- $auto = add1 $auto -}}{{- end -}}
{{- end -}}
{{- if gt $auto 1 -}}
{{- fail (printf "extraConfigs:最多一条 autoconfig: true,现在有 %d 条" $auto) -}}
{{- end -}}
{{- end -}}

{{/* 标了 autoconfig: true 的那条(没有则空)。 */}}
{{- define "cart.autoconfigEntry" -}}
{{- range $i, $e := .Values.extraConfigs -}}
{{- if $e.autoconfig -}}
{{- include "cart.extraConfig.resolve" (list $ $e $i) -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{/* reload watch 目录:reload.watchPaths 优先,其次 autoconfig 那条的挂载目录,最后退回底稿目录。 */}}
{{- define "cart.reload.watchPaths" -}}
{{- if .Values.reload.watchPaths -}}
{{- toYaml .Values.reload.watchPaths -}}
{{- else -}}
{{- $auto := include "cart.autoconfigEntry" . | fromYaml -}}
{{- if $auto.mountPath -}}
{{- toYaml (list $auto.mountPath) -}}
{{- else -}}
{{- toYaml (list "/workspace/configs") -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{/* 全部配置文件路径,空格分隔,按加载顺序。 */}}
{{- define "cart.configFiles" -}}
{{- $files := list "/workspace/configs/config.yaml" -}}
{{- range $i, $e := .Values.extraConfigs -}}
{{- $r := include "cart.extraConfig.resolve" (list $ $e $i) | fromYaml -}}
{{- $files = append $files $r.file -}}
{{- end -}}
{{- join " " $files -}}
{{- end -}}

{{/* 底稿 + 覆盖层的 volumeMounts。 */}}
{{- define "cart.configVolumeMounts" -}}
- name: cfg
  mountPath: /workspace/configs
{{- range $i, $e := .Values.extraConfigs }}
{{- $r := include "cart.extraConfig.resolve" (list $ $e $i) | fromYaml }}
- name: {{ $r.volume }}
  mountPath: {{ $r.mountPath }}
  readOnly: true
{{- end }}
{{- end -}}
