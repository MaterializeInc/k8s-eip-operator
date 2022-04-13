#!/usr/bin/python2.7
# I know, but it's what's available in the AWS CLI container, OK?
from __future__ import print_function
import subprocess

addresses = subprocess.check_output(
    [
        "aws",
        "--endpoint-url=http://localstack.default:4566",
        "ec2",
        "describe-addresses",
    ]
)
for a in addresses:
    print(a)


# aws --endpoint-url=http://localstack.default:4566 ec2 describe-addresses \
#     | jq -e '.Addresses[].Tags[] | select(.Key == "eip.materialize.cloud/eip_name").Value'
