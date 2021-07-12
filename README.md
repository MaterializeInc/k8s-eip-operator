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

## How to use this

1. TODO provide AWS IAM configs to grant permission to run this thing.
2. TODO provide K8S RBAC configs to grant permissions to run this thing.
3. TODO provide a yaml config for running this in K8S.
4. Add the `eip.aws.materialize.com/manage=true` label to any pods that you want this to manage.

## TODO
- [ ] Do not re-associate EIP to ENI/private IP if already associated with that ENI/private IP.
- [ ] Add Dockerfile.
- [ ] Determine K8S RBAC configs needed to run within K8S.
- [ ] Determine AWS IAM configs needed to run within K8S.
- [ ] Add CI integrations.
    - [ ] Build the binary.
    - [ ] Build the Docker image.
    - [ ] Push the image to Docker Hub.
- [ ] Refactor code for readability.
    - [ ] Create helper functions for getting values from kube and rusoto nested Option structs.
- [ ] Add doc comments.
- [ ] Update this README.md with missing documentation.

## References
* https://dzone.com/articles/oxidizing-the-kubernetes-operator
* https://github.com/LogMeIn/k8s-aws-operator
* https://github.com/kubernetes-sigs/external-dns/pull/2115/files
