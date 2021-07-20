# k8s-eip-operator

Manage external connections to Kubernetes pods using AWS Elastic IPs (EIPs).

This operator manages the following:
* Creation/destruction of EIP allocations in AWS.
    * These EIPs will be tagged with the pod uid they will be assigned to (`eip.aws.materialize.com/pod_uid`) for later identification.
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
                ],
                "Effect": "Allow",
                "Resource": "*",
            }
        ],
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
    ```

## Usage
Add the `eip.aws.materialize.com/manage=true` label to any pods that you want this to manage.

## TODO
- [X] Do not re-associate EIP to ENI/private IP if already associated with that ENI/private IP.
- [X] Add Dockerfile.
- [X] Determine K8S RBAC configs needed to run within K8S.
- [X] Determine AWS IAM configs needed to run within K8S.
- [X] Add CI integrations.
    - [X] Build the binary.
    - [X] Build the Docker image.
    - [X] Push the image to Docker Hub.
- [X] Add doc comments.
- [X] Update this README.md with missing documentation.

## References
* https://dzone.com/articles/oxidizing-the-kubernetes-operator
* https://github.com/LogMeIn/k8s-aws-operator
* https://github.com/kubernetes-sigs/external-dns/pull/2115/files
