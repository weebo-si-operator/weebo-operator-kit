{{/*
The operator Deployment: every replica serves the admission webhook and
the health routes (HTTPS, port `webhook`), the elected leader reconciles.
Takes (dict "ctx" $ "params" $params); params.env adds the operator's own
environment variables before values.extraEnv.
*/}}
{{- define "weebo-operator.deployment" -}}
{{- $ctx := .ctx -}}
{{- $p := .params -}}
{{- $v := $ctx.Values -}}
apiVersion: apps/v1
kind: Deployment
metadata:
  name: {{ include "weebo-operator.fullname" $ctx }}
  labels:
    {{- include "weebo-operator.labels" $ctx | nindent 4 }}
spec:
  replicas: {{ $v.replicaCount }}
  selector:
    matchLabels:
      {{- include "weebo-operator.selectorLabels" $ctx | nindent 6 }}
  template:
    metadata:
      labels:
        {{- include "weebo-operator.selectorLabels" $ctx | nindent 8 }}
        {{- with $v.podLabels }}
        {{- toYaml . | nindent 8 }}
        {{- end }}
      {{- with $v.podAnnotations }}
      annotations:
        {{- toYaml . | nindent 8 }}
      {{- end }}
    spec:
      serviceAccountName: {{ include "weebo-operator.serviceAccountName" $ctx }}
      {{- with $v.imagePullSecrets }}
      imagePullSecrets:
        {{- toYaml . | nindent 8 }}
      {{- end }}
      {{- with $v.priorityClassName }}
      priorityClassName: {{ . }}
      {{- end }}
      # The leader drains its in-flight reconciles before exiting.
      terminationGracePeriodSeconds: {{ $v.shutdown.terminationGracePeriodSeconds }}
      {{- with $v.podSecurityContext }}
      securityContext:
        {{- toYaml . | nindent 8 }}
      {{- end }}
      containers:
        - name: operator
          # Release images are tagged `v<version>` while appVersion carries
          # the bare version; image.digest wins.
          image: {{ include "weebo-operator.image" $ctx | quote }}
          imagePullPolicy: {{ $v.image.pullPolicy }}
          {{- with $v.securityContext }}
          securityContext:
            {{- toYaml . | nindent 12 }}
          {{- end }}
          ports:
            - name: webhook
              containerPort: {{ $v.webhook.containerPort }}
              protocol: TCP
            {{- if include "weebo-operator.metricsEnabled" $ctx }}
            - name: metrics
              containerPort: {{ $v.metrics.prometheus.port }}
              protocol: TCP
            {{- end }}
          env:
            {{- include "weebo-operator.env" (dict "ctx" $ctx "params" $p) | nindent 12 }}
          volumeMounts:
            # Default WEBHOOK_TLS_CERT_FILE/KEY_FILE location.
            - name: webhook-tls
              mountPath: /etc/webhook/certs
              readOnly: true
            # A Vault CA is materialized to a temp file for the Vault
            # client: /tmp must be writable under readOnlyRootFilesystem.
            - name: tmp
              mountPath: /tmp
          {{- with $v.probes }}
          startupProbe:
            httpGet:
              path: /readyz
              port: webhook
              scheme: HTTPS
            periodSeconds: {{ .startup.periodSeconds }}
            timeoutSeconds: {{ .startup.timeoutSeconds }}
            failureThreshold: {{ .startup.failureThreshold }}
          readinessProbe:
            httpGet:
              path: /readyz
              port: webhook
              scheme: HTTPS
            periodSeconds: {{ .readiness.periodSeconds }}
            timeoutSeconds: {{ .readiness.timeoutSeconds }}
            failureThreshold: {{ .readiness.failureThreshold }}
          livenessProbe:
            httpGet:
              path: /livez
              port: webhook
              scheme: HTTPS
            periodSeconds: {{ .liveness.periodSeconds }}
            timeoutSeconds: {{ .liveness.timeoutSeconds }}
            failureThreshold: {{ .liveness.failureThreshold }}
          {{- end }}
          {{- if gt (int $v.shutdown.preStopSleepSeconds) 0 }}
          # Native sleep action (Kubernetes 1.30+): the images have no shell.
          lifecycle:
            preStop:
              sleep:
                seconds: {{ $v.shutdown.preStopSleepSeconds }}
          {{- end }}
          {{- with $v.resources }}
          resources:
            {{- toYaml . | nindent 12 }}
          {{- end }}
      volumes:
        - name: webhook-tls
          secret:
            secretName: {{ include "weebo-operator.webhookTlsSecretName" $ctx }}
        - name: tmp
          emptyDir:
            sizeLimit: 64Mi
      {{- with $v.nodeSelector }}
      nodeSelector:
        {{- toYaml . | nindent 8 }}
      {{- end }}
      {{- with $v.tolerations }}
      tolerations:
        {{- toYaml . | nindent 8 }}
      {{- end }}
      {{- with $v.affinity }}
      affinity:
        {{- toYaml . | nindent 8 }}
      {{- end }}
      {{- if $v.topologySpreadConstraints }}
      topologySpreadConstraints:
        {{- range $v.topologySpreadConstraints }}
        {{- $c := deepCopy . }}
        {{- if not $c.labelSelector }}
        {{- $_ := set $c "labelSelector" (dict "matchLabels" (include "weebo-operator.selectorLabels" $ctx | fromYaml)) }}
        {{- end }}
        - {{ toYaml $c | nindent 10 | trim }}
        {{- end }}
      {{- else if $v.defaultTopologySpread }}
      topologySpreadConstraints:
        - maxSkew: 1
          topologyKey: kubernetes.io/hostname
          whenUnsatisfiable: ScheduleAnyway
          labelSelector:
            matchLabels:
              {{- include "weebo-operator.selectorLabels" $ctx | nindent 14 }}
      {{- end }}
{{- end -}}

{{/*
Only rendered when replicaCount > 1: with a single replica, minAvailable 1
would block every node drain.
*/}}
{{- define "weebo-operator.podDisruptionBudget" -}}
{{- $ctx := .ctx -}}
{{- if and $ctx.Values.podDisruptionBudget.enabled (gt (int $ctx.Values.replicaCount) 1) -}}
apiVersion: policy/v1
kind: PodDisruptionBudget
metadata:
  name: {{ include "weebo-operator.fullname" $ctx }}
  labels:
    {{- include "weebo-operator.labels" $ctx | nindent 4 }}
spec:
  minAvailable: {{ $ctx.Values.podDisruptionBudget.minAvailable }}
  # Replicas that are not Ready (crash loop) never block a node drain.
  unhealthyPodEvictionPolicy: AlwaysAllow
  selector:
    matchLabels:
      {{- include "weebo-operator.selectorLabels" $ctx | nindent 6 }}
{{- end -}}
{{- end -}}

{{/*
Optional NetworkPolicy. Ingress: the webhook port and the metrics port.
Egress is only restricted when networkPolicy.egress.enabled: DNS, the API
server, then params.egress (the remote system, as
`[{comment, rules}]`), networkPolicy.egress.vault and .extra.
networkPolicy.kind CiliumNetworkPolicy renders weebo-operator.ciliumNetworkPolicy
instead.
*/}}
{{- define "weebo-operator.networkPolicy" -}}
{{- $ctx := .ctx -}}
{{- $p := .params -}}
{{- $np := $ctx.Values.networkPolicy -}}
{{- if and $np.enabled (eq ($np.kind | default "NetworkPolicy") "CiliumNetworkPolicy") -}}
{{- include "weebo-operator.ciliumNetworkPolicy" . -}}
{{- else if $np.enabled -}}
{{- if and $np.egress.enabled (not $np.egress.apiServer.to) }}
{{- fail "networkPolicy.egress.apiServer.to is required when networkPolicy.egress.enabled: the operator cannot work without the API server" }}
{{- end -}}
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata:
  name: {{ include "weebo-operator.fullname" $ctx }}
  labels:
    {{- include "weebo-operator.labels" $ctx | nindent 4 }}
spec:
  podSelector:
    matchLabels:
      {{- include "weebo-operator.selectorLabels" $ctx | nindent 6 }}
  policyTypes:
    - Ingress
    {{- if $np.egress.enabled }}
    - Egress
    {{- end }}
  ingress:
    # Admission requests from the API server, and the probes (kubelet
    # traffic is not subject to NetworkPolicy on most CNIs).
    - ports:
        - port: webhook
          protocol: TCP
      {{- with $np.webhookFrom }}
      from:
        {{- toYaml . | nindent 8 }}
      {{- end }}
    {{- if include "weebo-operator.metricsEnabled" $ctx }}
    - ports:
        - port: metrics
          protocol: TCP
      {{- with $np.metricsFrom }}
      from:
        {{- toYaml . | nindent 8 }}
      {{- end }}
    {{- end }}
  {{- if $np.egress.enabled }}
  egress:
    {{- if $np.egress.dns.enabled }}
    - to:
        {{- toYaml $np.egress.dns.to | nindent 8 }}
      ports:
        {{- toYaml $np.egress.dns.ports | nindent 8 }}
    {{- end }}
    # Watches, Secrets, Leases, Events.
    - to:
        {{- toYaml $np.egress.apiServer.to | nindent 8 }}
      ports:
        {{- toYaml $np.egress.apiServer.ports | nindent 8 }}
    {{- range $e := $p.egress }}
    {{- if $e.rules }}
    {{- with $e.comment }}
    # {{ . }}
    {{- end }}
    {{- toYaml $e.rules | nindent 4 }}
    {{- end }}
    {{- end }}
    {{- with $np.egress.vault }}
    # Vault secret stores.
    {{- toYaml . | nindent 4 }}
    {{- end }}
    {{- with $np.egress.extra }}
    {{- toYaml . | nindent 4 }}
    {{- end }}
  {{- end }}
{{- end -}}
{{- end -}}

{{/*
The same policy as a CiliumNetworkPolicy (networkPolicy.kind), for Cilium:
there an `ipBlock` never selects the API server, whose identity is the
`kube-apiserver` entity, so the NetworkPolicy's egress would cut the
operator off. Ingress: the probes from the node (`host`) and webhookFrom
on the webhook port, metricsFrom on the metrics port; an empty list means
`fromEntities: [all]`. Egress (networkPolicy.egress.enabled): cilium.dnsTo
and cilium.apiServerTo on the ports of egress.dns and egress.apiServer,
then params.egress, egress.vault and .extra, written as Cilium rules.
A peer without a from* selector, or a rule without a to* one, is refused:
Cilium reads it as "from/to anywhere".
*/}}
{{- define "weebo-operator.ciliumNetworkPolicy" -}}
{{- $ctx := .ctx -}}
{{- $p := .params -}}
{{- $v := $ctx.Values -}}
{{- $np := $v.networkPolicy -}}
{{- $c := $np.cilium | default dict -}}
{{- $dnsTo := $c.dnsTo | default (dict "toEndpoints" (list (dict "matchLabels" (dict "k8s:io.kubernetes.pod.namespace" "kube-system" "k8s-app" "kube-dns")))) -}}
{{- $apiServerTo := $c.apiServerTo | default (dict "toEntities" (list "kube-apiserver")) -}}
{{- $all := list (dict "fromEntities" (list "all")) -}}
{{- $webhookPorts := list (dict "ports" (list (dict "port" (toString $v.webhook.containerPort) "protocol" "TCP"))) -}}
{{- $ingress := list -}}
{{- range $peer := $np.webhookFrom | default $all -}}
{{- $ingress = append $ingress (merge (dict "toPorts" $webhookPorts) $peer) -}}
{{- end -}}
{{- if include "weebo-operator.metricsEnabled" $ctx -}}
{{- $metricsPorts := list (dict "ports" (list (dict "port" (toString $v.metrics.prometheus.port) "protocol" "TCP"))) -}}
{{- range $peer := $np.metricsFrom | default $all -}}
{{- $ingress = append $ingress (merge (dict "toPorts" $metricsPorts) $peer) -}}
{{- end -}}
{{- end -}}
{{- range $i, $r := $ingress -}}
{{- $ok := false -}}
{{- range $k, $val := $r -}}
{{- if and (hasPrefix "from" $k) (ne $k "fromRequires") $val -}}{{- $ok = true -}}{{- end -}}
{{- end -}}
{{- if not $ok -}}
{{- fail (printf "networkPolicy.webhookFrom/metricsFrom: peer %d has no from* selector, Cilium would read it as from anywhere" $i) -}}
{{- end -}}
{{- end -}}
{{- /* Kubelet probes hit the webhook port from the node itself. */ -}}
{{- $ingress = prepend $ingress (dict "fromEntities" (list "host") "toPorts" $webhookPorts) -}}
{{- $egress := list -}}
{{- if $np.egress.enabled -}}
{{- if $np.egress.dns.enabled -}}
{{- $egress = append $egress (merge (dict "toPorts" (include "weebo-operator.ciliumToPorts" $np.egress.dns.ports | fromYamlArray)) $dnsTo) -}}
{{- end -}}
{{- $egress = append $egress (merge (dict "toPorts" (include "weebo-operator.ciliumToPorts" $np.egress.apiServer.ports | fromYamlArray)) $apiServerTo) -}}
{{- range $e := $p.egress -}}
{{- $egress = concat $egress ($e.rules | default list) -}}
{{- end -}}
{{- $egress = concat $egress ($np.egress.vault | default list) ($np.egress.extra | default list) -}}
{{- range $i, $r := $egress -}}
{{- $ok := false -}}
{{- range $k, $val := $r -}}
{{- if and (hasPrefix "to" $k) (ne $k "toPorts") (ne $k "toRequires") $val -}}{{- $ok = true -}}{{- end -}}
{{- end -}}
{{- if not $ok -}}
{{- fail (printf "networkPolicy egress rule %d (CiliumNetworkPolicy) has no to* selector, Cilium would read it as to anywhere" $i) -}}
{{- end -}}
{{- end -}}
{{- end -}}
apiVersion: cilium.io/v2
kind: CiliumNetworkPolicy
metadata:
  name: {{ include "weebo-operator.fullname" $ctx }}
  labels:
    {{- include "weebo-operator.labels" $ctx | nindent 4 }}
spec:
  endpointSelector:
    matchLabels:
      {{- include "weebo-operator.selectorLabels" $ctx | nindent 6 }}
  ingress:
    {{- toYaml $ingress | nindent 4 }}
  {{- with $egress }}
  egress:
    {{- toYaml . | nindent 4 }}
  {{- end }}
{{- end -}}

{{/* NetworkPolicy ports (`[{port, protocol}]`) as Cilium toPorts. */}}
{{- define "weebo-operator.ciliumToPorts" -}}
{{- $ports := list -}}
{{- range . -}}
{{- $ports = append $ports (dict "port" (toString .port) "protocol" (.protocol | default "TCP")) -}}
{{- end -}}
{{- toYaml (list (dict "ports" $ports)) -}}
{{- end -}}

{{/*
The container's environment: the kit's variables, then params.env, then
extraEnv. A name set again later wins and appears once (a duplicate key is
refused by server-side apply).
*/}}
{{- define "weebo-operator.env" -}}
{{- $ctx := .ctx -}}
{{- $p := .params -}}
{{- $v := $ctx.Values -}}
{{- $base := include "weebo-operator.kitEnv" $ctx | fromYamlArray -}}
{{- $all := concat $base ($p.env | default list) ($v.extraEnv | default list) -}}
{{- $out := list -}}
{{- range $i, $e := $all -}}
{{- $later := false -}}
{{- range $j, $f := $all -}}
{{- if and (gt $j $i) (eq (toString $f.name) (toString $e.name)) -}}
{{- $later = true -}}
{{- end -}}
{{- end -}}
{{- if not $later -}}
{{- $out = append $out $e -}}
{{- end -}}
{{- end -}}
{{- toYaml $out -}}
{{- end -}}

{{- define "weebo-operator.kitEnv" -}}
{{- $ctx := . -}}
{{- $v := $ctx.Values -}}
# Scopes the leader-election Lease; HOSTNAME (the pod name) is
# the holder identity.
- name: POD_NAMESPACE
  valueFrom:
    fieldRef:
      fieldPath: metadata.namespace
- name: LEASE_NAME
  value: {{ include "weebo-operator.leaseName" $ctx | quote }}
# The operator pod's own selector labels (JSON object, the Deployment's
# matchLabels): what a NetworkPolicy the operator writes selects to let
# the operator itself in.
- name: WEEBO_OPERATOR_POD_LABELS
  value: {{ include "weebo-operator.selectorLabels" $ctx | fromYaml | toJson | quote }}
- name: WEBHOOK_PORT
  value: {{ $v.webhook.containerPort | quote }}
- name: WEEBO_SHUTDOWN_DRAIN_SECONDS
  value: {{ $v.shutdown.drainSeconds | quote }}
{{- with include "weebo-operator.watchNamespace" $ctx }}
- name: WEEBO_WATCH_NAMESPACE
  value: {{ . | quote }}
{{- end }}
{{- if not (include "weebo-operator.watchClusterKinds" $ctx) }}
- name: WEEBO_WATCH_CLUSTER_KINDS
  value: "false"
{{- end }}
{{- with include "weebo-operator.watchLabelSelector" $ctx }}
- name: WEEBO_WATCH_LABEL_SELECTOR
  value: {{ . | quote }}
{{- end }}
{{- if (($ctx.Values.references | default dict).crossNamespace) }}
- name: WEEBO_CROSS_NAMESPACE_REFS
  value: "true"
{{- end }}
- name: WEEBO_RECONCILE_CONCURRENCY
  value: {{ $v.reconcileConcurrency | quote }}
{{- with $v.workerThreads }}
# tokio worker threads (the binary defaults to 4, not one per core).
- name: WEEBO_WORKER_THREADS
  value: {{ . | quote }}
{{- end }}
- name: LOG_FORMAT
  value: {{ $v.logFormat | quote }}
{{- if include "weebo-operator.metricsEnabled" $ctx }}
- name: WEEBO_METRICS_ADDR
  value: {{ printf "0.0.0.0:%v" $v.metrics.prometheus.port | quote }}
{{- end }}
{{- end -}}
