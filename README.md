# k8s-eip-operator

Manage external connections to Kubernetes pods using AWS Elastic IPs (EIPs).

This operator manages the following:
* Creation of an EIP Custom Resource Definition in K8S.
* Creation/destruction of EIP allocations in AWS.
    * These AWS EIPs will be tagged with the K8S EIP uid they will be assigned to (`eip.materialize.cloud/eip_uid`) for later identification.
* Association/disassociation of the EIP to the Elastic Network Interface (ENI) on the private IP of the pod.
* Annotation of `external-dns.alpha.kubernetes.io/target` on the pod, so that the `external-dns` operator knows to use the IP of the EIP instead of the external IP of the primary ENI on the host.

## Prerequisites

* You must be using the [AWS VPC CNI plugin](https://github.com/aws/amazon-vpc-cni-k8s). This is the default for AWS EKS, so you probably don't need to change anything here.

* You must set `AWS_VPC_K8S_CNI_EXTERNALSNAT=true` on the aws-node daemonset.
    ```
    kubectl set env daemonset -n kube-system aws-node AWS_VPC_K8S_CNI_EXTERNALSNAT=true
    ```
    See the [AWS external-snat docs](https://docs.aws.amazon.com/eks/latest/userguide/external-snat.html) for more details.

* For `external-dns` support, you must be using a version with headless ClusterIp support, either by waiting until [this PR is merged](https://github.com/kubernetes-sigs/external-dns/pull/2115) or by using a [fork with it already included](https://github.com/MaterializeInc/external-dns).

## Installation

1. Create an AWS IAM role with the following policy:
    ```json
    {
        "Version": "2012-10-17",
        "Statement": [
            {
                "Action": [
                    "ec2:AllocateAddress",
                    "ec2:ReleaseAddress",
                    "ec2:DescribeAddresses",
                    "ec2:AssociateAddress",
                    "ec2:DisassociateAddress",
                    "ec2:DescribeNetworkInterfaces",
                    "ec2:CreateTags",
                    "ec2:DeleteTags",
                    "ec2:DescribeInstances",
                    "ec2:ModifyNetworkInterfaceAttribute",
                    "servicequotas:GetServiceQuota"
                ],
                "Effect": "Allow",
                "Resource": "*"
            }
        ]
    }
    ```
2. Create a K8S ServiceAccount.
    ```yaml
    apiVersion: v1
    kind: ServiceAccount
    metadata:
      name: eip-operator
      annotations:
        eks.amazonaws.com/role-arn: arn:aws:iam::ACCOUNT-ID:role/IAM-SERVICE-ROLE-NAME
    ```
3. Create a K8S ClusterRole.
    ```yaml
    apiVersion: rbac.authorization.k8s.io/v1
    kind: ClusterRole
    metadata:
      name: eip-operator
    rules:
    - apiGroups: [""]
      resources: ["pods", "pods/status"]
      verbs: ["get", "watch", "list", "update", "patch"]
    - apiGroups: [""]
      resources: ["nodes", "nodes/status"]
      verbs: ["get"]
    ```
4. Create a K8S ClusterRoleBinding.
    ```yaml
    apiVersion: rbac.authorization.k8s.io/v1
    kind: ClusterRoleBinding
    metadata:
      name: eip-operator-viewer
    roleRef:
      apiGroup: rbac.authorization.k8s.io
      kind: ClusterRole
      name: eip-operator
    subjects:
    - kind: ServiceAccount
      name: eip-operator
      namespace: default
    ```
5. Create a K8S Deployment.
You must specify the `CLUSTER_NAME` environment variable. `NAMESPACE` and `DEFAULT_TAGS` are optional.
If you do not set the `NAMESPACE` environment variable, the eip-operator will operate on all namespaces.
Even in this global mode, the eip and pod must be in the same namespace.
    ```yaml
    apiVersion: apps/v1
    kind: Deployment
    metadata:
      name: eip-operator
    spec:
      strategy:
        type: Recreate
      selector:
        matchLabels:
          app: eip-operator
      template:
        metadata:
          labels:
            app: eip-operator
        spec:
          containers:
          - name: eip-operator
            image: materialize/k8s-eip-operator:latest
            env:
            - name: NAMESPACE
              value: default
            - name: CLUSTER_NAME
              value: my-example-cluster
            - name: DEFAULT_TAGS
              value: '{"tag1": "value1", "tag2": "value2"}'
    ```

## Usage

##### A. If you want your EIP to survive beyond the lifetime of the pod (ie: for static reservations when updating a statefulset):

Instantiate an Eip Kubernetes object, specifying the `podName` in the spec.
```yaml
apiVersion: "materialize.cloud/v1"
kind: Eip
metadata:
  name: my-new-eip
spec:
  podName: my-pod
```

Add the `eip.materialize.cloud/manage=true` label to the pod with name matching the `podName` specified above.

##### B. If you don't care about getting a new IP if the pod gets recreated:

No need to manually create the Eip in this case, the operator can do it for you.

Add both the `eip.materialize.cloud/manage=true` and `eip.materialize.cloud/autocreate_eip=true` labels to your pod.

Do NOT manually create the Eip Kubernetes object if setting the `eip.materialize.cloud/autocreate_eip=true` label, or the two objects will fight over your pod.

### Testing

- Install the [KUTTL](https://kuttl.dev/docs/) testing tool
- Run `./bin/run-tests`

## OpenTelemetry Integration

We now have support for sending traces using the OpenTelemetry OTLP format. This is configured through environment variables:

`OPENTELEMETRY_ENDPOINT` is the endpoint to send the logs to.

`OPENTELEMETRY_HEADERS` is a json formatted map of key/value pairs to be included in the GRPC request headers.

`OPENTELEMETRY_TOPLEVEL_FIELDS` is a json formatted map of key/value pairs to be included in all trace spans emitted from the service.

`OPENTELEMETRY_LEVEL_TARGETS` is a [`tracing` crate level filter](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html). Defaults to `"DEBUG"`.

`OPENTELEMETRY_SAMPLE_RATE` is a float value controlling the trace sample rate. Default is 0.05.


## References
* https://dzone.com/articles/oxidizing-the-kubernetes-operator
* https://github.com/LogMeIn/k8s-aws-operator
* https://github.com/kubernetes-sigs/external-dns/pull/2115/files
